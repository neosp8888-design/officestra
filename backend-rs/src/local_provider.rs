// 로컬 4090 모델 제공자의 정의·표시·재개 규칙과 턴 기록을 맡는다. Node local-provider-service.mjs 앞부분과 같다.
// 다리·모델 풀·앞문·원격 호스트는 하위 모듈에 있다. 주소·키 경로·비밀값은 로그에 남기지 않는다.

pub mod bridge;
pub mod host;
pub mod llama;
pub mod pool;
pub mod requests;
pub mod service;
pub mod tools;
pub mod usage;

use std::fmt;
use std::sync::Arc;

use serde_json::{Map, Value, json};
use sqlx::{PgConnection, Row};
use tokio::sync::watch;

use crate::js::js_truthy;
use crate::local_agent_profile::{LocalAgentProfile, normalize_local_agent_profile};
use crate::local_model_capabilities::{
    DIRECT_RUNTIME, MEROMERO_26B_MODEL_ID, direct_codex_reasoning, direct_default_reasoning, direct_reasoning_options, is_mero_mero,
    local_model_supports_vision,
};

pub use host::{LocalHost, validate_host};

/// 로컬 제공자 오류. 다리는 status가 있으면 그 응답 코드로, 없으면 502로 돌려준다.
/// busy는 Node LocalHostBusyError(그래픽 메모리 점유는 그 하위 LocalHostGPUOccupiedError)다.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalError {
    pub status: Option<u16>,
    pub message: String,
    pub busy: Option<BusyDetail>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BusyDetail {
    Host,
    /// 앱이 ComfyUI 정리 방식을 고르게 넘기는 {vramPct, ramPct, comfyUI}.
    GpuOccupied(Value),
}

impl LocalError {
    pub fn plain(message: impl Into<String>) -> Self {
        Self { status: None, message: message.into(), busy: None }
    }

    pub fn status(status: u16, message: impl Into<String>) -> Self {
        Self { status: Some(status), message: message.into(), busy: None }
    }

    pub fn busy(message: impl Into<String>) -> Self {
        Self { status: None, message: message.into(), busy: Some(BusyDetail::Host) }
    }

    pub fn is_busy(&self) -> bool {
        self.busy.is_some()
    }

    /// LocalHostGPUOccupiedError의 gpu 필드. 다른 오류는 None이다.
    pub fn gpu(&self) -> Option<&Value> {
        match &self.busy {
            Some(BusyDetail::GpuOccupied(gpu)) => Some(gpu),
            _ => None,
        }
    }
}

impl fmt::Display for LocalError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl From<String> for LocalError {
    fn from(message: String) -> Self {
        Self::plain(message)
    }
}

/// AbortController.signal. 한 번 중단되면 되돌릴 수 없다.
#[derive(Clone)]
pub struct AbortSignal(Arc<watch::Sender<bool>>);

impl Default for AbortSignal {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for AbortSignal {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "AbortSignal({})", self.is_aborted())
    }
}

impl AbortSignal {
    pub fn new() -> Self {
        Self(Arc::new(watch::channel(false).0))
    }

    pub fn abort(&self) {
        self.0.send_replace(true);
    }

    pub fn is_aborted(&self) -> bool {
        *self.0.borrow()
    }

    /// 중단될 때 끝난다. 이미 중단됐으면 바로 끝난다.
    pub async fn aborted(&self) {
        let mut receiver = self.0.subscribe();
        let _ = receiver.wait_for(|aborted| *aborted).await;
    }
}

/// 검증한 로컬 정의. host는 Node `{...h}`처럼 받은 필드를 모두 지닌다.
#[derive(Debug, Clone, PartialEq)]
pub struct LocalDefinition {
    pub profile: LocalAgentProfile,
    pub host: LocalHost,
}

impl LocalDefinition {
    /// JSON.stringify(definition). 턴 스냅숏과 재개 비교에 쓴다.
    pub fn to_value(&self) -> Value {
        json!({ "profile": self.profile, "host": self.host.to_value() })
    }

    pub fn is_direct(&self) -> bool {
        self.profile.runtime.as_deref() == Some(DIRECT_RUNTIME)
    }

    /// 주소·추론 선택을 덮어쓴다(Node assignedLocalDefinition·resolveLocalCharacter). 빈 값은 무시한다.
    pub fn assigned(&self, address: Option<&str>, reasoning: Option<&str>) -> Result<Self, String> {
        let mut next = self.clone();
        if let Some(address) = address.filter(|address| !address.is_empty()) {
            next.host = next.host.with_address(address)?;
        }
        if let Some(reasoning) = reasoning.filter(|reasoning| !reasoning.is_empty()) {
            let mut profile = serde_json::to_value(&next.profile).map_err(|error| error.to_string())?;
            profile["reasoning"] = json!(reasoning);
            next.profile = normalize_local_agent_profile(&profile)?;
        }
        Ok(next)
    }
}

/// normalizeLocalDefinition.
pub fn normalize_local_definition(value: &Value) -> Result<LocalDefinition, String> {
    let profile = normalize_local_agent_profile(value.get("profile").unwrap_or(&Value::Null))?;
    let host = validate_host(value.get("host").unwrap_or(&Value::Null))?;
    Ok(LocalDefinition { profile, host })
}

/// localProfileTitle. 호스트 키는 경로 식별자라 실제 적재 모델과 다를 수 있다.
pub fn local_profile_title(definition: &LocalDefinition) -> String {
    let profile = &definition.profile;
    if is_mero_mero(profile) {
        let variant = if profile.model == MEROMERO_26B_MODEL_ID { "26B A4B · 비전" } else { "31B · 텍스트 전용" };
        return format!("G4 MeroMero {variant} Uncensored Heretic · llama.cpp");
    }
    let name = if profile.model == "officestra-qwen38-27b-uncensored-q4km" {
        "qwen3.8-27b-uncensored".to_string()
    } else {
        definition.host.model_key().rsplit('/').next().unwrap_or_default().to_string()
    };
    let runtime = if definition.is_direct() { " · llama.cpp · MTP" } else { " · LM Studio" };
    format!("{name}{runtime}")
}

/// localProfileReasoningOptions.
pub fn local_profile_reasoning_options(definition: &LocalDefinition) -> Vec<&'static str> {
    if definition.is_direct() {
        direct_reasoning_options(&definition.profile).to_vec()
    } else if host::is_qwen38_lmstudio_model_key(definition.host.model_key()) {
        vec!["default", "on", "off"]
    } else {
        Vec::new()
    }
}

/// localProfileDefaultReasoning.
pub fn local_profile_default_reasoning(definition: &LocalDefinition) -> &'static str {
    if definition.is_direct() { direct_default_reasoning(&definition.profile) } else { "default" }
}

/// localCodexModelCatalog. Codex는 사용자 지정 제공자에게 모델 정보를 묻는다. 고정한 이 모델만 알리고 클라우드 등급은 알리지 않는다.
pub fn local_codex_model_catalog(profile: &LocalAgentProfile) -> Value {
    let mero = is_mero_mero(profile);
    let direct = profile.runtime.as_deref() == Some(DIRECT_RUNTIME);
    let family = if mero { "MeroMero" } else { "Qwen" };
    let default_reasoning = if direct {
        let mut copy = profile.clone();
        copy.reasoning = Some("default".into());
        json!(direct_codex_reasoning(&copy))
    } else {
        Value::Null
    };
    let levels = if mero {
        json!([{ "effort": "none", "description": "Thinking off" }, { "effort": "medium", "description": "Thinking on (native toggle)" }])
    } else if direct {
        Value::Array(["low", "medium", "xhigh"].iter().map(|effort| json!({ "effort": effort, "description": format!("Qwen {effort}") })).collect())
    } else {
        json!([])
    };
    let modalities = if local_model_supports_vision(profile) { json!(["text", "image"]) } else { json!(["text"]) };
    json!({ "models": [{
        "slug": profile.model,
        "display_name": profile.model,
        "description": format!("Local {family} on your 4090"),
        "default_reasoning_level": default_reasoning,
        "supported_reasoning_levels": levels,
        "shell_type": "unified_exec",
        "visibility": "list",
        "supported_in_api": true,
        "priority": 1,
        "additional_speed_tiers": [],
        "service_tiers": [],
        "availability_nux": null,
        "upgrade": null,
        "base_instructions": format!("You are a coding assistant running a local {family} model through Codex. Use the available tools to complete the user request. Follow tool documentation and do not invent tool results."),
        "model_messages": null,
        "default_reasoning_summary": "none",
        "support_verbosity": false,
        "default_verbosity": null,
        "apply_patch_tool_type": "freeform",
        "web_search_tool_type": "text",
        // Codex는 4바이트를 1토큰으로 어림한다. 한국어·JSON 출력은 로컬 토크나이저에서 약 2.5배 촘촘하다.
        "truncation_policy": { "mode": "tokens", "limit": 2500 },
        "supports_image_detail_original": false,
        "context_window": profile.context_window,
        "max_context_window": profile.context_window,
        "effective_context_window_percent": 100,
        "experimental_supported_tools": [],
        "input_modalities": modalities,
        "supports_search_tool": true,
        "supports_experimental_context": false,
    }] })
}

/// localResumeCompatible. 주소는 경로일 뿐 세션 정체가 아니다. 같은 고정 호스트 키일 때만 주소를 비교에서 뺀다.
pub fn local_resume_compatible(previous: &Value, next: &Value) -> bool {
    let alias = |value: &Value| value.get("host").and_then(|host| host.get("hostKeyAlias")).and_then(Value::as_str).map(str::to_string);
    let same_pinned_host = alias(previous).is_some_and(|previous_alias| !previous_alias.is_empty() && Some(&previous_alias) == alias(next).as_ref());
    let strip = |value: &Value| -> Value {
        let Some(fields) = value.as_object().filter(|_| js_truthy(Some(value))) else {
            return value.clone();
        };
        let mut next = fields.clone();
        let host = fields.get("host").cloned().unwrap_or(Value::Null);
        let host = if same_pinned_host {
            let mut stripped = host.as_object().cloned().unwrap_or_default();
            stripped.shift_remove("address");
            Value::Object(stripped)
        } else {
            host
        };
        next.insert("host".into(), host);
        let mut profile: Map<String, Value> = fields.get("profile").and_then(Value::as_object).cloned().unwrap_or_default();
        profile.shift_remove("reasoning");
        profile.shift_remove("maxOutputTokens");
        next.insert("profile".into(), Value::Object(profile));
        Value::Object(next)
    };
    let previous = strip(previous);
    let next = strip(next);
    if previous == next {
        return true;
    }
    // 측정한 문맥 창 증가는 모델·계정·세션 정체를 바꾸지 않는다. 살아 있는 연결에는 쓰지 않는다.
    let window = |value: &Value| value.get("profile").and_then(|profile| profile.get("contextWindow")).and_then(Value::as_f64);
    if window(&previous) != Some(32768.0) || window(&next) != Some(65536.0) {
        return false;
    }
    let mut widened = previous;
    widened["profile"]["contextWindow"] = json!(65536);
    widened == next
}

/// resolveLocalCharacter. 로컬 직원이 아니면 None, 정의가 꺼졌거나 설정이 다르면 오류다.
pub async fn resolve_local_character(
    connection: &mut PgConnection,
    config: &Value,
    backend: &str,
    model: Option<&str>,
    effort: &str,
    fast_mode: bool,
) -> Result<Option<LocalDefinition>, LocalResolveError> {
    let Some(id) = config.get("localProfileId").filter(|id| js_truthy(Some(id))) else {
        return Ok(None);
    };
    let row = sqlx::query("SELECT definition FROM local_agent_profiles WHERE id = $1 AND enabled = true")
        .bind(crate::js::js_string(id))
        .fetch_optional(&mut *connection)
        .await?;
    let Some(row) = row else {
        return Err(LocalResolveError::Invalid("Local profile is disabled or missing".into()));
    };
    let definition: Value = row.try_get("definition")?;
    let definition = normalize_local_definition(&definition).map_err(LocalResolveError::Invalid)?;
    let text = |key: &str| config.get(key).filter(|value| js_truthy(Some(value))).map(crate::js::js_string);
    let definition = definition.assigned(text("localHostAddress").as_deref(), text("localReasoning").as_deref()).map_err(LocalResolveError::Invalid)?;
    if backend != definition.profile.backend || model != Some(definition.profile.model.as_str()) || effort != "default" || fast_mode {
        return Err(LocalResolveError::Invalid("Local employee settings do not match the profile".into()));
    }
    Ok(Some(definition))
}

#[derive(Debug)]
pub enum LocalResolveError {
    Invalid(String),
    Database(sqlx::Error),
}

impl fmt::Display for LocalResolveError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Invalid(message) => formatter.write_str(message),
            Self::Database(error) => write!(formatter, "{error}"),
        }
    }
}

impl From<sqlx::Error> for LocalResolveError {
    fn from(error: sqlx::Error) -> Self {
        Self::Database(error)
    }
}

/// validateLocalResume. 이어 쓸 외부 세션이 있으면 마지막 턴도 로컬이고 호환돼야 한다.
pub async fn validate_local_resume(
    connection: &mut PgConnection,
    session_id: &str,
    local_profile: Option<&Value>,
    external_id: Option<&str>,
) -> Result<(), LocalResolveError> {
    let (Some(local_profile), Some(_)) = (local_profile, external_id.filter(|id| !id.is_empty())) else {
        return Ok(());
    };
    let row = sqlx::query("SELECT provider_kind, provider_snapshot FROM turns WHERE cli_session_id = $1::uuid ORDER BY started_at DESC LIMIT 1")
        .bind(session_id)
        .fetch_optional(&mut *connection)
        .await?;
    let compatible = match row {
        Some(row) => {
            let kind: Option<String> = row.try_get("provider_kind")?;
            let snapshot: Option<Value> = row.try_get("provider_snapshot")?;
            kind.as_deref() == Some("local") && local_resume_compatible(&snapshot.unwrap_or(Value::Null), local_profile)
        }
        None => false,
    };
    if !compatible {
        return Err(LocalResolveError::Invalid("Start a new session before switching between cloud and local providers".into()));
    }
    Ok(())
}

/// snapshotLocalTurn. local_profile은 직원의 character.localProfile(정의 JSON)이다.
pub async fn snapshot_local_turn(connection: &mut PgConnection, turn_id: &str, local_profile: Option<&Value>) -> Result<(), sqlx::Error> {
    let Some(local_profile) = local_profile else {
        return Ok(());
    };
    sqlx::query("UPDATE turns SET provider_kind = 'local', provider_snapshot = $2::jsonb WHERE id = $1::uuid")
        .bind(turn_id)
        .bind(local_profile)
        .execute(&mut *connection)
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::local_model_capabilities::{MEROMERO_26B_HOST_KEY, MEROMERO_HOST_KEY, MEROMERO_MODEL_ID};

    pub(crate) fn mero_definition(model: &str, host_key: &str, backend: &str, context: u64) -> LocalDefinition {
        let protocol = if backend == "claude" { "anthropic-normalized-v1" } else { "openai-responses-v1" };
        normalize_local_definition(&json!({
            "profile": { "id": "mero-test", "providerKind": "local", "backend": backend, "runtime": DIRECT_RUNTIME, "model": model,
                "endpoint": "http://127.0.0.1:41235", "credentialEnv": "OFFICESTRA_LOCAL_TEST_TOKEN", "credentialVersion": "v1",
                "contextWindow": context, "maxOutputTokens": 8192, "usageProtocol": protocol, "kvCacheQuantization": "q8_0", "reasoning": "default" },
            "host": { "address": "127.0.0.1", "user": "test", "sshPort": 2222, "keyPath": "/tmp/test-key", "hostKeyAlias": "test", "modelKey": host_key, "comfyPort": 8188 },
        }))
        .unwrap()
    }

    #[test]
    fn titles_and_reasoning_menus() {
        let mero = mero_definition(MEROMERO_MODEL_ID, MEROMERO_HOST_KEY, "codex", 32768);
        assert!(local_profile_title(&mero).contains("MeroMero"));
        assert!(!local_profile_title(&mero).contains("MTP"));
        assert_eq!(local_profile_reasoning_options(&mero), ["default", "off", "on"]);
        assert_eq!(local_profile_default_reasoning(&mero), "off");
        let model = &local_codex_model_catalog(&mero.profile)["models"][0];
        assert_eq!(model["input_modalities"], json!(["text"]));
        assert_eq!(model["default_reasoning_level"], "none");
        assert_eq!(model["supported_reasoning_levels"].as_array().unwrap().iter().map(|level| level["effort"].clone()).collect::<Vec<_>>(), [json!("none"), json!("medium")]);
        let vision = mero_definition(MEROMERO_26B_MODEL_ID, MEROMERO_26B_HOST_KEY, "codex", 65536);
        assert!(local_profile_title(&vision).contains("26B A4B"));
        assert_eq!(local_codex_model_catalog(&vision.profile)["models"][0]["input_modalities"], json!(["text", "image"]));
        assert!(!local_resume_compatible(&mero.to_value(), &vision.to_value()));

        let lmstudio = normalize_local_definition(&json!({
            "profile": { "id": "lm", "providerKind": "local", "backend": "claude", "model": "model", "endpoint": "http://127.0.0.1:41235",
                "credentialEnv": "OFFICESTRA_LOCAL_TEST_TOKEN", "credentialVersion": "v1", "contextWindow": 32768, "maxOutputTokens": 4096, "usageProtocol": "anthropic-normalized-v1" },
            "host": { "address": "127.0.0.1", "user": "test", "sshPort": 2222, "keyPath": "/tmp/key", "hostKeyAlias": "a", "modelKey": "vendor/model", "comfyPort": 8188 },
        }))
        .unwrap();
        assert_eq!(local_profile_title(&lmstudio), "model · LM Studio");
        assert!(local_profile_reasoning_options(&lmstudio).is_empty());
        let catalog = local_codex_model_catalog(&lmstudio.profile);
        assert_eq!(catalog["models"][0]["default_reasoning_level"], Value::Null);
        assert_eq!(catalog["models"][0]["truncation_policy"], json!({ "mode": "tokens", "limit": 2500 }));
        assert_eq!(catalog["models"][0]["effective_context_window_percent"], 100);
    }

    #[test]
    fn resume_compatibility_rules() {
        let previous = mero_definition(MEROMERO_MODEL_ID, MEROMERO_HOST_KEY, "codex", 32768);
        let next = mero_definition(MEROMERO_MODEL_ID, MEROMERO_HOST_KEY, "codex", 65536);
        assert!(local_resume_compatible(&previous.to_value(), &next.to_value()));
        assert!(!local_resume_compatible(&next.to_value(), &previous.to_value()));
        let moved = previous.assigned(Some("10.0.0.2"), Some("on")).unwrap();
        assert!(local_resume_compatible(&previous.to_value(), &moved.to_value()));
        let mut unpinned = previous.to_value();
        unpinned["host"]["hostKeyAlias"] = json!("");
        let mut unpinned_moved = unpinned.clone();
        unpinned_moved["host"]["address"] = json!("10.0.0.2");
        assert!(!local_resume_compatible(&unpinned, &unpinned_moved));
        let mut other_model = previous.to_value();
        other_model["profile"]["model"] = json!("officestra-qwen38-27b-uncensored-q4km");
        assert!(!local_resume_compatible(&previous.to_value(), &other_model));
        assert!(!local_resume_compatible(&Value::Null, &previous.to_value()));
    }

    #[tokio::test]
    async fn abort_signal_wakes_waiters_once() {
        let signal = AbortSignal::new();
        let waiter = tokio::spawn({
            let signal = signal.clone();
            async move { signal.aborted().await }
        });
        assert!(!signal.is_aborted());
        signal.abort();
        waiter.await.unwrap();
        signal.aborted().await;
        assert!(signal.is_aborted());
    }
}
