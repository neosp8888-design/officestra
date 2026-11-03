// 직원의 로컬 모델 선택·주소·추론·실행 제어와 /api/local-profiles 목록·저장. Node local-profile-selection.mjs와 server.mjs 로컬 경로를 옮겼다.

use std::fmt;
use std::net::Ipv4Addr;
use std::time::Duration;

use futures_util::future::BoxFuture;
use serde_json::{Map, Value, json};
use sqlx::{Connection, PgConnection, PgPool, Row};

use crate::character_settings::{CharacterSettingsError, with_character_session_locks};
use crate::health::{Runtime, SessionEndPlan};
use crate::js::js_trim;
use crate::local_model_capabilities::{
    DIRECT_RUNTIME, LocalProfileLike, MEROMERO_26B_MODEL_ID, direct_default_reasoning, direct_reasoning_options, is_mero_mero,
};
use crate::local_provider::host::{COMFY_RELEASE_MODES, RemoteShell, SshShell, is_qwen38_lmstudio_model_key};
use crate::local_provider::service::LocalProviderService;
use crate::local_provider::{LocalDefinition, LocalError, local_profile_reasoning_options, normalize_local_definition};

/// 작업 중이거나 터미널이 열린 직원. Node 문구 그대로다.
pub const LOCAL_SELECTION_BUSY: &str = "이 직원의 작업을 마치고 터미널을 닫은 뒤 전환하세요.";
const STATE_CHANGED: &str = "직원 상태가 바뀌었습니다. 작업 종료 후 다시 선택하세요.";
const BACKEND_NOT_READY: &str = "백엔드가 준비된 뒤 다시 선택하세요.";
const INVALID_ADDRESS: &str = "올바른 IPv4 주소를 입력하세요. 포트나 http://는 넣지 마세요.";
const ADDRESS_UNREACHABLE: &str = "해당 IP의 기존 PC에 SSH 연결할 수 없습니다. 주소와 PC 상태를 확인하세요.";
const WORKER_CLOSED: &str = "Claude Code 지속 세션을 종료했습니다.";
const FINALIZE_WARNING: &str = "설정은 저장됐지만 이전 작업 공간 정리 상태를 확인해야 합니다.";
/// 경로가 먼저 거르는 추론 선택지. 프로필별 허용 목록은 저장 직전에 다시 본다.
pub const LOCAL_REASONING_OPTIONS: [&str; 6] = ["default", "on", "off", "low", "medium", "xhigh"];

#[derive(Debug)]
pub enum LocalSelectionError {
    /// Node AgentBusyError.
    Busy(String),
    /// Node GitWorkspaceError. 작업 공간 정리 중에만 난다.
    Workspace(String),
    /// 로컬 제공자 오류. busy면 Node LocalHostBusyError(그래픽 메모리 점유 포함)다.
    Local(LocalError),
    /// Node 일반 Error·TypeError.
    Failed(String),
    Database(sqlx::Error),
}

impl LocalSelectionError {
    fn busy(message: &str) -> Self {
        Self::Busy(message.to_string())
    }

    fn failed(message: impl Into<String>) -> Self {
        Self::Failed(message.into())
    }

    /// AgentBusyError 또는 LocalHostBusyError.
    pub fn is_busy(&self) -> bool {
        match self {
            Self::Busy(_) => true,
            Self::Local(error) => error.is_busy(),
            _ => false,
        }
    }

    /// local-model·local-address 경로. 바쁨은 409, 나머지는 400이다.
    pub fn route_status(&self) -> u16 {
        if self.is_busy() { 409 } else { 400 }
    }

    /// local-reasoning·local-profile 경로는 서버 공통 처리로 간다. AgentBusyError·GitWorkspaceError만 409, 나머지는 500이다.
    pub fn generic_status(&self) -> u16 {
        match self {
            Self::Busy(_) | Self::Workspace(_) => 409,
            _ => 500,
        }
    }

    /// LocalHostGPUOccupiedError의 gpu.
    pub fn gpu(&self) -> Option<&Value> {
        match self {
            Self::Local(error) => error.gpu(),
            _ => None,
        }
    }
}

impl fmt::Display for LocalSelectionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Busy(message) | Self::Workspace(message) | Self::Failed(message) => formatter.write_str(message),
            Self::Local(error) => write!(formatter, "{error}"),
            Self::Database(error) => write!(formatter, "{error}"),
        }
    }
}

impl From<sqlx::Error> for LocalSelectionError {
    fn from(error: sqlx::Error) -> Self {
        Self::Database(error)
    }
}

impl From<LocalError> for LocalSelectionError {
    fn from(error: LocalError) -> Self {
        Self::Local(error)
    }
}

impl From<CharacterSettingsError> for LocalSelectionError {
    fn from(error: CharacterSettingsError) -> Self {
        match error {
            CharacterSettingsError::Busy(message) => Self::Busy(message),
            CharacterSettingsError::Workspace(message) => Self::Workspace(message),
            CharacterSettingsError::Database(error) => Self::Database(error),
            other => Self::Failed(other.to_string()),
        }
    }
}

/// 이 모듈이 쓰는 로컬 제공자 기능. 시험은 가짜로 바꾼다(Node localProviders 주입).
pub trait LocalModelControl: Send + Sync {
    fn control_model<'a>(&'a self, raw: &'a Value, action: &'a str, comfy_release: Option<&'a str>) -> BoxFuture<'a, Result<Value, LocalError>>;
    fn close_idle_profile<'a>(&'a self, profile_id: &'a str, verified_address: Option<&'a str>) -> BoxFuture<'a, Result<(), LocalError>>;
}

impl LocalModelControl for LocalProviderService {
    fn control_model<'a>(&'a self, raw: &'a Value, action: &'a str, comfy_release: Option<&'a str>) -> BoxFuture<'a, Result<Value, LocalError>> {
        Box::pin(LocalProviderService::control_model(self, raw, action, comfy_release))
    }

    fn close_idle_profile<'a>(&'a self, profile_id: &'a str, verified_address: Option<&'a str>) -> BoxFuture<'a, Result<(), LocalError>> {
        Box::pin(LocalProviderService::close_idle_profile(self, profile_id, verified_address))
    }
}

/// verifyHost. 저장 전에 새 주소의 PC가 고정된 호스트 키로 SSH에 답하는지 본다.
pub type VerifyHost = dyn Fn(LocalDefinition) -> BoxFuture<'static, Result<(), String>> + Send + Sync;

/// 기본 verifyHost. 원격 출력은 쓰지 않고 성공 여부만 본다.
pub fn verify_host_by_ssh(definition: LocalDefinition) -> BoxFuture<'static, Result<(), String>> {
    let probe = SshShell.run(&definition.host, "Write-Output OFFICESTRA_ADDRESS_OK".to_string(), Duration::from_millis(10_000), None);
    Box::pin(async move { probe.await.map(|_| ()).map_err(|_| ADDRESS_UNREACHABLE.to_string()) })
}

/// Node 참 판정. 빈 문자열·0·false·null·없음만 거짓이다.
fn is_truthy(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) => false,
        Some(Value::Bool(flag)) => *flag,
        Some(Value::String(text)) => !text.is_empty(),
        Some(Value::Number(number)) => number.as_f64().is_some_and(|n| n != 0.0 && !n.is_nan()),
        Some(_) => true,
    }
}

/// normalizeLocalHostAddress. Node isIP()===4처럼 앞자리 0·255 초과·포트·주소 외 글자를 거절한다.
pub fn normalize_local_host_address(value: &Value) -> Result<String, String> {
    let address = value.as_str().map(js_trim).unwrap_or("");
    if address.parse::<Ipv4Addr>().is_err() {
        return Err(INVALID_ADDRESS.to_string());
    }
    Ok(address.to_string())
}

/// localPermission. 이전 클라우드 권한의 수준을 로컬 실행기 권한으로 옮긴다.
pub fn local_permission(previous: Option<&Value>, backend: &str) -> &'static str {
    let previous = previous.and_then(Value::as_str).unwrap_or("");
    let codex = backend == "codex";
    if ["danger-full-access", "dangerously-skip-permissions", "bypassPermissions"].contains(&previous) {
        return if codex { "danger-full-access" } else { "bypassPermissions" };
    }
    if ["workspace-write", "accept-edits", "acceptEdits", "auto"].contains(&previous) {
        return if codex { "workspace-write" } else { "auto" };
    }
    if codex { "read-only" } else { "plan" }
}

fn config_of(previous: &Value) -> Map<String, Value> {
    previous.get("config").and_then(Value::as_object).cloned().unwrap_or_default()
}

/// localSelectionSettings. 정의가 없으면 저장해 둔 클라우드 설정으로 되돌리고, 있으면 그 프로필로 고정한다.
/// 바꿀 것이 없으면 None이다. 첫 로컬 전환 때만 클라우드 복귀 지점을 저장한다.
pub fn local_selection_settings(previous: &Value, definition: Option<&LocalDefinition>) -> Result<Option<Value>, String> {
    let mut config = config_of(previous);
    let Some(definition) = definition else {
        if !is_truthy(config.get("localProfileId")) {
            return Ok(None);
        }
        let saved = config.get("localPreviousSettings").cloned();
        if !is_truthy(saved.as_ref()) {
            return Err("Previous cloud settings are unavailable".to_string());
        }
        let saved = saved.unwrap_or_default();
        for key in ["localProfileId", "localPreviousSettings", "localReasoning", "localHostAddress", "executablePath"] {
            config.shift_remove(key);
        }
        if is_truthy(saved.get("executablePath")) {
            config.insert("executablePath".into(), saved["executablePath"].clone());
        }
        let mut restored = saved.as_object().cloned().unwrap_or_default();
        let percent = saved.get("autoCompactPercent").filter(|value| !value.is_null()).cloned().unwrap_or(json!(90));
        restored.insert("autoCompactPercent".into(), percent);
        restored.insert("config".into(), Value::Object(config));
        return Ok(Some(Value::Object(restored)));
    };
    let profile = &definition.profile;
    if config.get("localProfileId") == Some(&Value::String(profile.id.clone())) {
        return Ok(None);
    }
    let backend = profile.backend.as_str();
    if !["claude", "codex"].contains(&backend) {
        return Err("Unsupported local runner backend".to_string());
    }
    let was_local = is_truthy(config.get("localProfileId"));
    if !was_local {
        // JSON.stringify처럼 없는 필드는 빼고, null은 그대로 둔다.
        let mut saved = Map::new();
        for (target, source) in [("backend", "backend"), ("model", "model"), ("effort", "effort"), ("fastMode", "fastMode"), ("permission", "permission")] {
            if let Some(value) = previous.get(source) {
                saved.insert(target.into(), value.clone());
            }
        }
        let percent = previous.get("autoCompactPercent").filter(|value| !value.is_null()).cloned().unwrap_or(json!(90));
        saved.insert("autoCompactPercent".into(), percent);
        saved.insert("executablePath".into(), config.get("executablePath").cloned().unwrap_or(Value::Null));
        config.insert("localPreviousSettings".into(), Value::Object(saved));
    }
    config.shift_remove("executablePath");
    let local_address = if was_local { config.get("localHostAddress").cloned() } else { None };
    config.insert("localProfileId".into(), json!(profile.id));
    config.shift_remove("localReasoning");
    config.shift_remove("localHostAddress");
    // 바뀌는 것은 실행기이고 사용자의 PC 경로가 아니다. 새 프로필의 고정 호스트 키는 모델 요청 전에 SSH가 다시 확인한다.
    if is_truthy(local_address.as_ref()) {
        let address = normalize_local_host_address(local_address.as_ref().unwrap_or(&Value::Null))?;
        config.insert("localHostAddress".into(), json!(address));
    }
    Ok(Some(json!({
        "backend": backend,
        "model": profile.model,
        "effort": "default",
        "fastMode": false,
        "permission": local_permission(previous.get("permission"), backend),
        "autoCompactPercent": 100,
        "config": config,
    })))
}

/// 실행·압축 중이거나 터미널이 열린 직원. include_preparing이면 다른 준비 중인 직원도 바쁘다고 본다.
fn character_active(runtime: &Runtime, character_id: &str, include_preparing: bool) -> bool {
    let terminal = runtime.terminal_sessions().is_some_and(|sessions| sessions.has(character_id));
    let jobs = runtime.jobs();
    terminal
        || jobs.running.contains_key(character_id)
        || jobs.compacting.contains(character_id)
        || (include_preparing && jobs.preparing_characters.contains(character_id))
}

/// 잠금 안의 재확인. 자기 준비 등록은 빼고 본다.
fn state_changed(runtime: &Runtime, character_id: &str) -> bool {
    runtime.is_draining() || character_active(runtime, character_id, false)
}

/// preparingCharacters 등록. 어떻게 끝나든 지운다(Node finally).
struct Preparing<'a> {
    runtime: &'a Runtime,
    character_id: String,
}

impl<'a> Preparing<'a> {
    /// 바쁘지 않으면 등록한다. 실행표 확인과 등록은 같은 잠금 안에서 한다.
    fn enter(runtime: &'a Runtime, character_id: &str) -> Result<Self, LocalSelectionError> {
        let terminal = runtime.terminal_sessions().is_some_and(|sessions| sessions.has(character_id));
        let mut jobs = runtime.jobs();
        if terminal
            || jobs.running.contains_key(character_id)
            || jobs.compacting.contains(character_id)
            || jobs.preparing_characters.contains(character_id)
        {
            return Err(LocalSelectionError::busy(LOCAL_SELECTION_BUSY));
        }
        jobs.preparing_characters.insert(character_id.to_string());
        Ok(Self { runtime, character_id: character_id.to_string() })
    }
}

impl Drop for Preparing<'_> {
    fn drop(&mut self) {
        self.runtime.jobs().preparing_characters.remove(&self.character_id);
    }
}

/// 잠금을 잡은 연결에서 트랜잭션 하나로 본문을 돌린다. 실패하면 되돌리고 원래 오류를 올린다.
async fn in_transaction<T>(
    pool: &PgPool,
    character_id: &str,
    body: impl AsyncFnOnce(&mut PgConnection) -> Result<T, LocalSelectionError>,
) -> Result<T, LocalSelectionError> {
    let ids = [character_id.to_string()];
    with_character_session_locks(pool, &ids, async |connection: &mut PgConnection| {
        let mut transaction = connection.begin().await?;
        match body(&mut *transaction).await {
            Ok(value) => {
                transaction.commit().await?;
                Ok(value)
            }
            Err(error) => {
                transaction.rollback().await.ok();
                Err(error)
            }
        }
    })
    .await
}

async fn locked_config(connection: &mut PgConnection, character_id: &str) -> Result<Map<String, Value>, LocalSelectionError> {
    let config: Option<Option<Value>> = sqlx::query_scalar("SELECT config FROM characters WHERE id=$1 FOR UPDATE")
        .bind(character_id)
        .fetch_optional(&mut *connection)
        .await?;
    Ok(config.flatten().and_then(|config| config.as_object().cloned()).unwrap_or_default())
}

/// 켜진 프로필 정의. 없으면 Null이라 정규화가 Node처럼 오류를 낸다.
async fn shared_definition(connection: &mut PgConnection, profile_id: &str) -> Result<Option<Value>, LocalSelectionError> {
    Ok(sqlx::query_scalar("SELECT definition FROM local_agent_profiles WHERE id=$1 AND enabled=true FOR SHARE")
        .bind(profile_id)
        .fetch_optional(&mut *connection)
        .await?)
}

/// 문자열 프로필 ID. 그 밖의 값은 선택이 없는 것으로 본다.
fn profile_id_of(config: &Map<String, Value>) -> Option<String> {
    config.get("localProfileId").and_then(Value::as_str).filter(|id| !id.is_empty()).map(str::to_string)
}

/// assignedLocalDefinition. 원본 정의를 정규화하고 주소·추론 선택을 덮어쓴다.
fn assigned_definition(raw: &Value, address: Option<&str>, reasoning: Option<&str>) -> Result<LocalDefinition, LocalSelectionError> {
    normalize_local_definition(raw)
        .and_then(|definition| definition.assigned(address, reasoning))
        .map_err(LocalSelectionError::Failed)
}

/// controlLocalModel. 직원이 고른 주소·추론으로 모델을 올리거나 내린다. 배정과 세션은 건드리지 않는다.
/// 중지하면 같은 PC의 롤플레이 로컬 모델도 잠그지 않고 내린다.
pub async fn control_local_model(
    pool: &PgPool,
    runtime: &Runtime,
    local_providers: Option<&dyn LocalModelControl>,
    character_id: &str,
    action: Option<&str>,
    comfy_release: Option<&Value>,
) -> Result<Value, LocalSelectionError> {
    if runtime.is_draining() {
        return Err(LocalSelectionError::busy(BACKEND_NOT_READY));
    }
    let Some(action) = action.filter(|action| ["start", "stop"].contains(action)) else {
        return Err(LocalSelectionError::failed("Unsupported model action"));
    };
    let comfy_release = match comfy_release {
        None => None,
        Some(Value::String(mode)) if action == "start" && COMFY_RELEASE_MODES.contains(&mode.as_str()) => Some(mode.as_str()),
        Some(_) => return Err(LocalSelectionError::failed("Unsupported ComfyUI release mode")),
    };
    let row = sqlx::query(
        "SELECT p.definition, c.config->>'localHostAddress' AS address,
           c.config->>'localReasoning' AS reasoning, r.local_profile_id AS roleplay_profile_id,
           rp.definition AS roleplay_definition FROM characters c
           JOIN local_agent_profiles p ON p.id=c.config->>'localProfileId'
           LEFT JOIN roleplay_profiles r ON r.character_id=c.id
           LEFT JOIN local_agent_profiles rp ON rp.id=r.local_profile_id AND rp.enabled=true
           WHERE c.id=$1 AND p.enabled=true",
    )
    .bind(character_id)
    .fetch_optional(pool)
    .await?;
    let Some(row) = row else {
        return Err(LocalSelectionError::failed("로컬 모델을 먼저 선택하세요."));
    };
    let raw: Value = row.try_get("definition")?;
    let address: Option<String> = row.try_get("address")?;
    let reasoning: Option<String> = row.try_get("reasoning")?;
    let roleplay_profile_id: Option<String> = row.try_get("roleplay_profile_id")?;
    let roleplay_definition: Option<Value> = row.try_get("roleplay_definition")?;
    let definition = assigned_definition(&raw, address.as_deref(), reasoning.as_deref())?;
    let Some(providers) = local_providers else {
        return Err(LocalSelectionError::failed("Local provider service unavailable"));
    };
    let status = providers.control_model(&definition.to_value(), action, comfy_release).await?;
    // 롤플레이는 메시지가 올 때 자기 로컬 모델을 올리고 계속 붙잡는다. 중지 버튼이 그 모델도 내려야 그래픽 메모리가 빈다(2026-10-21).
    // 주소는 직원 설정을, 추론은 같은 프로필일 때만 따른다(롤플레이 localDefinition과 같은 규칙).
    if action == "stop"
        && let Some(roleplay) = roleplay_definition.filter(|definition| !definition.is_null())
    {
        let same_profile = roleplay_profile_id.as_deref() == Some(definition.profile.id.as_str());
        let roleplay = assigned_definition(&roleplay, address.as_deref(), if same_profile { reasoning.as_deref() } else { None })?;
        providers.control_model(&roleplay.to_value(), "unload", None).await?;
    }
    let mut result = Map::new();
    result.insert("ok".into(), json!(true));
    if let Value::Object(fields) = status {
        result.extend(fields);
    }
    Ok(Value::Object(result))
}

/// setLocalHostAddress. 새 주소의 PC를 확인한 뒤 직원 config에만 저장한다. 세션과 기록은 그대로 둔다.
pub async fn set_local_host_address(
    pool: &PgPool,
    runtime: &Runtime,
    local_providers: Option<&dyn LocalModelControl>,
    character_id: &str,
    address: &Value,
    verify_host: &VerifyHost,
    broadcast: &(dyn Fn(Value) + Send + Sync),
) -> Result<Value, LocalSelectionError> {
    let address = normalize_local_host_address(address).map_err(LocalSelectionError::Failed)?;
    if runtime.is_draining() {
        return Err(LocalSelectionError::busy(LOCAL_SELECTION_BUSY));
    }
    let preparing = Preparing::enter(runtime, character_id)?;
    in_transaction(pool, character_id, async |connection: &mut PgConnection| {
        let mut config = locked_config(connection, character_id).await?;
        let Some(profile_id) = profile_id_of(&config) else {
            return Err(LocalSelectionError::failed("Local profile required"));
        };
        let raw = shared_definition(connection, &profile_id).await?.unwrap_or(Value::Null);
        let mut definition = normalize_local_definition(&raw).map_err(LocalSelectionError::Failed)?;
        definition.host = definition.host.with_address(&address).map_err(LocalSelectionError::Failed)?;
        verify_host(definition).await.map_err(LocalSelectionError::Failed)?;
        if state_changed(runtime, character_id) {
            return Err(LocalSelectionError::busy(LOCAL_SELECTION_BUSY));
        }
        if let Some(providers) = local_providers {
            providers.close_idle_profile(&profile_id, Some(&address)).await?;
        }
        config.insert("localHostAddress".into(), json!(address));
        sqlx::query("UPDATE characters SET config=$2::jsonb,updated_at=now() WHERE id=$1")
            .bind(character_id)
            .bind(Value::Object(config))
            .execute(&mut *connection)
            .await?;
        Ok(())
    })
    .await?;
    runtime.close_claude_worker(character_id, None, WORKER_CLOSED);
    broadcast(json!({ "type": "local.changed", "characterId": character_id }));
    drop(preparing);
    Ok(json!({ "ok": true, "characterId": character_id, "address": address }))
}

/// setLocalReasoning. 유휴 직원의 로컬 추론 선택만 바꾼다. 프로필이 허용하는 값만 저장한다.
pub async fn set_local_reasoning(
    pool: &PgPool,
    runtime: &Runtime,
    character_id: &str,
    reasoning: &Value,
    broadcast: &(dyn Fn(Value) + Send + Sync),
) -> Result<Value, LocalSelectionError> {
    let Some(reasoning) = reasoning.as_str().filter(|value| LOCAL_REASONING_OPTIONS.contains(value)) else {
        return Err(LocalSelectionError::failed("Unsupported local reasoning option"));
    };
    if runtime.is_draining() {
        return Err(LocalSelectionError::busy(LOCAL_SELECTION_BUSY));
    }
    let preparing = Preparing::enter(runtime, character_id)?;
    in_transaction(pool, character_id, async |connection: &mut PgConnection| {
        if state_changed(runtime, character_id) {
            return Err(LocalSelectionError::busy(STATE_CHANGED));
        }
        let mut config = locked_config(connection, character_id).await?;
        let Some(profile_id) = profile_id_of(&config) else {
            return Err(LocalSelectionError::failed("Local profile required"));
        };
        let raw = shared_definition(connection, &profile_id).await?.unwrap_or(Value::Null);
        let definition = normalize_local_definition(&raw).map_err(LocalSelectionError::Failed)?;
        if !local_profile_reasoning_options(&definition).contains(&reasoning) {
            return Err(LocalSelectionError::failed("Unsupported local reasoning option for this runtime"));
        }
        config.insert("localReasoning".into(), json!(reasoning));
        sqlx::query("UPDATE characters SET config=$2::jsonb,updated_at=now() WHERE id=$1")
            .bind(character_id)
            .bind(Value::Object(config))
            .execute(&mut *connection)
            .await?;
        Ok(())
    })
    .await?;
    runtime.close_claude_worker(character_id, None, WORKER_CLOSED);
    broadcast(json!({ "type": "local.changed", "characterId": character_id }));
    drop(preparing);
    Ok(json!({ "ok": true, "characterId": character_id, "reasoning": reasoning }))
}

/// JSON 값을 문자열 열에 넣는다. 문자열은 그대로, null·없음은 NULL, 그 밖은 JSON 표기다(pg가 받는 문자열과 같다).
fn text_column(value: Option<&Value>) -> Option<String> {
    match value {
        None | Some(Value::Null) => None,
        Some(Value::String(text)) => Some(text.clone()),
        Some(other) => Some(other.to_string()),
    }
}

/// selectLocalProfile. 로컬 프로필로 고정하거나(profile_id) 클라우드 설정으로 되돌린다(None).
/// 바뀌면 이전 세션을 같은 트랜잭션에서 끝내고, 커밋 뒤 작업 공간을 정리한다. 기록은 지우지 않는다.
pub async fn select_local_profile(
    pool: &PgPool,
    runtime: &Runtime,
    local_providers: Option<&dyn LocalModelControl>,
    character_id: &str,
    profile_id: Option<&str>,
    broadcast: &(dyn Fn(Value) + Send + Sync),
) -> Result<Value, LocalSelectionError> {
    if runtime.is_draining() {
        return Err(LocalSelectionError::busy(BACKEND_NOT_READY));
    }
    let profile_id = profile_id.filter(|id| !id.is_empty());
    let preparing = Preparing::enter(runtime, character_id)?;
    let (mut result, plan) = in_transaction(pool, character_id, async |connection: &mut PgConnection| {
        if state_changed(runtime, character_id) {
            return Err(LocalSelectionError::busy(STATE_CHANGED));
        }
        let row = sqlx::query(
            "SELECT id,backend,model,effort,fast_mode,permission,auto_compact_percent,config FROM characters WHERE id=$1 FOR UPDATE",
        )
        .bind(character_id)
        .fetch_optional(&mut *connection)
        .await?;
        let Some(row) = row else {
            return Err(LocalSelectionError::failed("직원을 찾을 수 없습니다."));
        };
        let current = json!({
            "id": row.try_get::<String, _>("id")?,
            "backend": row.try_get::<Option<String>, _>("backend")?,
            "model": row.try_get::<Option<String>, _>("model")?,
            "effort": row.try_get::<Option<String>, _>("effort")?,
            "fastMode": row.try_get::<Option<bool>, _>("fast_mode")?,
            "permission": row.try_get::<Option<String>, _>("permission")?,
            "autoCompactPercent": row.try_get::<Option<i16>, _>("auto_compact_percent")?,
            "config": row.try_get::<Option<Value>, _>("config")?,
        });
        let definition = match profile_id {
            Some(profile_id) => {
                let Some(raw) = shared_definition(connection, profile_id).await? else {
                    return Err(LocalSelectionError::failed("사용 가능한 로컬 모델이 없습니다."));
                };
                Some(normalize_local_definition(&raw).map_err(LocalSelectionError::Failed)?)
            }
            None => None,
        };
        let next = local_selection_settings(&current, definition.as_ref()).map_err(LocalSelectionError::Failed)?;
        let mut plan = None;
        if let Some(next) = &next {
            if let Some(previous_profile) = profile_id_of(&config_of(&current))
                && let Some(providers) = local_providers
            {
                providers.close_idle_profile(&previous_profile, None).await?;
            }
            let session_end = runtime.inspect_workspace_for_session_end(character_id, connection).await?;
            runtime.apply_workspace_session_end_plan(connection, &session_end).await?;
            let percent = next.get("autoCompactPercent").and_then(Value::as_i64).and_then(|percent| i16::try_from(percent).ok());
            sqlx::query(
                "UPDATE characters SET backend=$2,model=$3,effort=$4,fast_mode=$5,permission=$6,config=$7::jsonb,auto_compact_percent=$8,updated_at=now() WHERE id=$1",
            )
            .bind(character_id)
            .bind(text_column(next.get("backend")))
            .bind(text_column(next.get("model")))
            .bind(text_column(next.get("effort")))
            .bind(next.get("fastMode").and_then(Value::as_bool))
            .bind(text_column(next.get("permission")))
            .bind(next.get("config").cloned().unwrap_or(Value::Null))
            .bind(percent)
            .execute(&mut *connection)
            .await?;
            plan = Some(session_end);
        }
        let result = json!({ "ok": true, "characterId": character_id, "localProfileId": profile_id, "newSession": next.is_some() });
        Ok((result, plan))
    })
    .await?;
    if let Some(plan) = plan {
        finish_session_end(runtime, character_id, &plan, &mut result).await;
    }
    broadcast(json!({ "type": "local.changed", "characterId": character_id }));
    broadcast(json!({ "type": "session.changed", "characterId": character_id }));
    drop(preparing);
    Ok(result)
}

/// 커밋 뒤 정리. 실패는 경고 문구로만 남긴다(Node catch). 정리 쪽 자체 경고는 Node처럼 버린다.
async fn finish_session_end(runtime: &Runtime, character_id: &str, plan: &SessionEndPlan, result: &mut Value) {
    runtime.close_claude_worker(character_id, None, WORKER_CLOSED);
    if runtime.finalize_workspace_session_end_plan(plan).await.is_err() {
        result["warning"] = json!(FINALIZE_WARNING);
    }
}

/// 원본 정의의 profile 조각. Node 목록은 정규화하지 않은 원본으로 제목·추론 선택지를 만든다.
/// 그래서 지금 규칙을 통과하지 못하는 옛 비활성 프로필도 목록에 나온다.
struct RawProfile<'a> {
    model: &'a str,
    runtime: Option<&'a str>,
}

impl LocalProfileLike for RawProfile<'_> {
    fn model(&self) -> &str {
        self.model
    }
    fn runtime(&self) -> Option<&str> {
        self.runtime
    }
    fn reasoning(&self) -> Option<&str> {
        None
    }
}

const INVALID_DEFINITION: &str = "Invalid local profile definition";

fn raw_profile(definition: &Value) -> Result<(&Map<String, Value>, RawProfile<'_>), String> {
    let Some(profile) = definition.get("profile").and_then(Value::as_object) else {
        return Err(INVALID_DEFINITION.to_string());
    };
    let model = profile.get("model").and_then(Value::as_str).unwrap_or("");
    let runtime = profile.get("runtime").and_then(Value::as_str);
    Ok((profile, RawProfile { model, runtime }))
}

fn raw_model_key(definition: &Value) -> Option<&str> {
    definition.pointer("/host/modelKey").and_then(Value::as_str)
}

/// localProfileTitle(원본).
fn raw_profile_title(definition: &Value) -> Result<String, String> {
    let (_, profile) = raw_profile(definition)?;
    if is_mero_mero(&profile) {
        let variant = if profile.model == MEROMERO_26B_MODEL_ID { "26B A4B · 비전" } else { "31B · 텍스트 전용" };
        return Ok(format!("G4 MeroMero {variant} Uncensored Heretic · llama.cpp"));
    }
    // 호스트 키는 경로 식별자라 실제 적재 모델과 다를 수 있다.
    let name = if profile.model == "officestra-qwen38-27b-uncensored-q4km" {
        "qwen3.8-27b-uncensored".to_string()
    } else {
        let Some(key) = raw_model_key(definition) else {
            return Err(INVALID_DEFINITION.to_string());
        };
        key.rsplit('/').next().unwrap_or_default().to_string()
    };
    let runtime = if profile.runtime == Some(DIRECT_RUNTIME) { " · llama.cpp · MTP" } else { " · LM Studio" };
    Ok(format!("{name}{runtime}"))
}

/// localProfileReasoningOptions(원본).
fn raw_reasoning_options(definition: &Value) -> Result<Vec<&'static str>, String> {
    let (_, profile) = raw_profile(definition)?;
    Ok(if profile.runtime == Some(DIRECT_RUNTIME) {
        direct_reasoning_options(&profile).to_vec()
    } else if raw_model_key(definition).is_some_and(is_qwen38_lmstudio_model_key) {
        vec!["default", "on", "off"]
    } else {
        Vec::new()
    })
}

/// localProfileDefaultReasoning(원본).
fn raw_default_reasoning(definition: &Value) -> Result<&'static str, String> {
    let (_, profile) = raw_profile(definition)?;
    Ok(if profile.runtime == Some(DIRECT_RUNTIME) { direct_default_reasoning(&profile) } else { "default" })
}

/// GET /api/local-profiles. 프로필 요약, 직원 배정(주소 포함), 배정된 직원별 모델 상태다.
pub async fn list_local_profiles(pool: &PgPool, local_providers: Option<&LocalProviderService>) -> Result<Value, String> {
    let profiles = sqlx::query("SELECT id, enabled, definition FROM local_agent_profiles ORDER BY id")
        .fetch_all(pool)
        .await
        .map_err(|error| error.to_string())?;
    let assignments = sqlx::query(
        "SELECT c.id AS \"characterId\", c.config->>'localProfileId' AS \"profileId\", c.config->>'localReasoning' AS reasoning, COALESCE(c.config->>'localHostAddress', p.definition->'host'->>'address') AS address FROM characters c LEFT JOIN local_agent_profiles p ON p.id=c.config->>'localProfileId' WHERE c.config ? 'localProfileId'",
    )
    .fetch_all(pool)
    .await
    .map_err(|error| error.to_string())?;
    let mut profile_rows = Vec::with_capacity(profiles.len());
    for row in &profiles {
        let id: String = row.try_get("id").map_err(|error| error.to_string())?;
        let enabled: bool = row.try_get("enabled").map_err(|error| error.to_string())?;
        let definition: Value = row.try_get("definition").map_err(|error| error.to_string())?;
        profile_rows.push((id, enabled, definition));
    }
    let mut assignment_rows = Vec::with_capacity(assignments.len());
    for row in &assignments {
        let read = |name: &str| row.try_get::<Option<String>, _>(name).map_err(|error| error.to_string());
        assignment_rows.push(json!({
            "characterId": read("characterId")?,
            "profileId": read("profileId")?,
            "reasoning": read("reasoning")?,
            "address": read("address")?,
        }));
    }
    let mut statuses = Vec::new();
    if let Some(providers) = local_providers {
        for assignment in &assignment_rows {
            let Some((_, _, raw)) = profile_rows.iter().find(|(id, _, _)| Some(id.as_str()) == assignment["profileId"].as_str()) else {
                continue;
            };
            let definition = assigned_definition(raw, assignment["address"].as_str(), assignment["reasoning"].as_str())
                .map_err(|error| error.to_string())?;
            let mut status = Map::new();
            status.insert("id".into(), assignment["characterId"].clone());
            status.insert("characterId".into(), assignment["characterId"].clone());
            status.insert("profileId".into(), assignment["profileId"].clone());
            if let Value::Object(fields) = providers.model_status(&definition).map_err(|error| error.to_string())? {
                status.extend(fields);
            }
            statuses.push(Value::Object(status));
        }
    }
    let mut summaries = Vec::with_capacity(profile_rows.len());
    for (id, enabled, definition) in &profile_rows {
        let (profile, _) = raw_profile(definition)?;
        let mut summary = Map::new();
        summary.insert("id".into(), json!(id));
        summary.insert("enabled".into(), json!(enabled));
        // JSON.stringify처럼 원본에 없는 필드는 뺀다.
        for (target, source) in [("backend", "backend"), ("model", "model")] {
            if let Some(value) = profile.get(source) {
                summary.insert(target.into(), value.clone());
            }
        }
        summary.insert("title".into(), json!(raw_profile_title(definition)?));
        if let Some(value) = profile.get("contextWindow") {
            summary.insert("contextWindow".into(), value.clone());
        }
        summary.insert("kvCacheQuantization".into(), profile.get("kvCacheQuantization").cloned().unwrap_or(Value::Null));
        summary.insert("reasoningOptions".into(), json!(raw_reasoning_options(definition)?));
        summary.insert("defaultReasoning".into(), json!(raw_default_reasoning(definition)?));
        summaries.push(Value::Object(summary));
    }
    Ok(json!({ "profiles": summaries, "assignments": assignment_rows, "statuses": statuses }))
}

/// PUT /api/local-profiles. 정규화한 정의를 저장한다. 그 프로필의 앞문이 열려 있으면 409다.
/// 오류는 (응답 코드, 문구)다. 정규화 실패는 Node 공통 처리처럼 500이다.
pub async fn save_local_profile(
    pool: &PgPool,
    local_providers: Option<&LocalProviderService>,
    body: &Value,
) -> Result<Value, (u16, String)> {
    let definition = normalize_local_definition(body.get("definition").unwrap_or(&Value::Null)).map_err(|message| (500, message))?;
    let profile_id = json!(definition.profile.id);
    let open = local_providers.is_some_and(|providers| {
        providers.status().iter().any(|status| {
            status.get("profileId").filter(|value| !value.is_null()).or_else(|| status.get("id")) == Some(&profile_id)
        })
    });
    if open {
        return Err((409, "Close local sessions before changing a profile".to_string()));
    }
    sqlx::query(
        "INSERT INTO local_agent_profiles(id,definition,enabled) VALUES($1,$2::jsonb,$3) ON CONFLICT(id) DO UPDATE SET definition=EXCLUDED.definition,enabled=EXCLUDED.enabled,updated_at=now()",
    )
    .bind(&definition.profile.id)
    .bind(definition.to_value())
    .bind(body.get("enabled") == Some(&Value::Bool(true)))
    .execute(pool)
    .await
    .map_err(|error| (500, error.to_string()))?;
    Ok(json!({ "id": definition.profile.id }))
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};

    use tokio::sync::OnceCell;

    use super::*;
    use crate::agent_runtime::launch::RuntimeCharacter;
    use crate::agent_runtime::turn::{TurnHandle, TurnSeed, TurnState};
    use crate::local_provider::{BusyDetail, local_profile_title};

    /// 문서용 주소(TEST-NET). 실제 PC 주소를 시험에 쓰지 않는다.
    const ADDRESS: &str = "198.51.100.7";

    fn definition_value(id: &str) -> Value {
        json!({
            "profile": {
                "id": id, "providerKind": "local", "backend": "claude", "model": "qwen-test", "endpoint": "http://127.0.0.1:41235",
                "credentialEnv": "OFFICESTRA_LOCAL_TEST_TOKEN", "credentialVersion": "v1", "contextWindow": 32768, "maxOutputTokens": 4096,
                "usageProtocol": "anthropic-normalized-v1",
            },
            "host": {
                "address": "192.0.2.10", "user": "test", "sshPort": 2222, "keyPath": "/tmp/key", "hostKeyAlias": "test",
                "modelKey": "qwen/test", "comfyPort": 8188,
            },
        })
    }

    fn changed(mut value: Value, edit: impl FnOnce(&mut Value)) -> Value {
        edit(&mut value);
        value
    }

    fn definition_of(value: &Value) -> LocalDefinition {
        normalize_local_definition(value).unwrap()
    }

    fn original() -> Value {
        json!({
            "id": "test", "backend": "codex", "model": "gpt-test", "effort": "high", "fastMode": true, "permission": "workspace-write",
            "config": { "executablePath": "/custom/codex", "keep": "unchanged" },
        })
    }

    fn with(mut value: Value, key: &str, field: Value) -> Value {
        value[key] = field;
        value
    }

    fn settings(previous: &Value, definition: &LocalDefinition) -> Value {
        local_selection_settings(previous, Some(definition)).unwrap().unwrap()
    }

    #[test]
    fn ipv4_input_rejects_commands_ports_urls_invalid_octets_and_empty_values() {
        assert_eq!(normalize_local_host_address(&json!(" 198.51.100.7 ")).unwrap(), ADDRESS);
        for value in [
            json!(""),
            Value::Null,
            json!(123),
            json!("host"),
            json!("127.0.0.1:22"),
            json!("http://127.0.0.1"),
            json!("1.2.3.256"),
            json!("01.2.3.4"),
            json!("1.2.3.4;whoami"),
            json!("::1"),
        ] {
            assert_eq!(normalize_local_host_address(&value), Err(INVALID_ADDRESS.to_string()), "{value}");
        }
    }

    #[test]
    fn local_selection_pins_measured_model_and_restores_exact_previous_cloud_settings() {
        let original = original();
        let before = original.clone();
        let definition = definition_of(&definition_value("local-test"));
        let local = settings(&original, &definition);
        assert_eq!(local["autoCompactPercent"], 100);
        assert_eq!(
            (&local["backend"], &local["model"], &local["effort"], &local["fastMode"], &local["permission"]),
            (&json!("claude"), &json!("qwen-test"), &json!("default"), &json!(false), &json!("auto"))
        );
        assert!(local["config"].get("executablePath").is_none());
        assert_eq!(local["config"]["keep"], "unchanged");
        assert_eq!(
            local["config"]["localPreviousSettings"],
            json!({
                "backend": "codex", "model": "gpt-test", "effort": "high", "fastMode": true, "permission": "workspace-write",
                "autoCompactPercent": 90, "executablePath": "/custom/codex",
            })
        );
        let restored = local_selection_settings(&local, None).unwrap().unwrap();
        assert_eq!(restored["autoCompactPercent"], 90);
        for key in ["backend", "model", "effort", "fastMode", "permission", "config"] {
            assert_eq!(restored[key], original[key], "{key}");
        }
        assert_eq!(original, before);
        assert_eq!(local_selection_settings(&local, Some(&definition)).unwrap(), None);
        assert_eq!(local_selection_settings(&original, None).unwrap(), None);
    }

    #[test]
    fn local_selection_uses_the_profile_runner_and_maps_permissions_for_that_cli() {
        let original = original();
        let base = definition_of(&definition_value("local-test"));
        let mut codex = base.clone();
        codex.profile.id = "local-codex".into();
        codex.profile.backend = "codex".into();
        assert_eq!(settings(&original, &codex)["backend"], "codex");
        assert_eq!(settings(&original, &codex)["permission"], "workspace-write");
        assert_eq!(settings(&with(original.clone(), "permission", json!("danger-full-access")), &codex)["permission"], "danger-full-access");
        assert_eq!(settings(&with(original.clone(), "permission", json!("plan")), &codex)["permission"], "read-only");
        assert_eq!(settings(&with(original.clone(), "permission", json!("dangerously-skip-permissions")), &base)["permission"], "bypassPermissions");
        assert_eq!(settings(&with(original.clone(), "permission", json!("read-only")), &base)["permission"], "plan");
        let mut antigravity = base;
        antigravity.profile.backend = "antigravity".into();
        assert_eq!(local_selection_settings(&original, Some(&antigravity)), Err("Unsupported local runner backend".to_string()));
    }

    #[test]
    fn selecting_another_local_profile_does_not_overwrite_cloud_restore_point() {
        let base = definition_of(&definition_value("local-test"));
        let mut first = settings(&original(), &base);
        let mut second = base.clone();
        second.profile.id = "second".into();
        second.profile.model = "other".into();
        assert_eq!(settings(&first, &second)["config"]["localPreviousSettings"], first["config"]["localPreviousSettings"]);
        first["config"]["localHostAddress"] = json!(ADDRESS);
        let mut codex = base;
        codex.profile.id = "codex-profile".into();
        codex.profile.backend = "codex".into();
        assert_eq!(settings(&first, &codex)["config"]["localHostAddress"], ADDRESS);
        assert!(local_selection_settings(&json!({ "config": { "localProfileId": "missing" } }), None).is_err());
    }

    #[test]
    fn profile_summary_reads_raw_definitions_even_when_current_rules_reject_them() {
        let lm_studio = definition_value("local-test");
        assert_eq!(raw_profile_title(&lm_studio).unwrap(), local_profile_title(&definition_of(&lm_studio)));
        assert_eq!(raw_profile_title(&lm_studio).unwrap(), "test · LM Studio");
        assert!(raw_reasoning_options(&lm_studio).unwrap().is_empty());
        assert_eq!(raw_default_reasoning(&lm_studio).unwrap(), "default");
        let rejected = changed(lm_studio.clone(), |value| value["profile"]["legacy"] = json!(true));
        assert!(normalize_local_definition(&rejected).is_err());
        assert_eq!(raw_profile_title(&rejected).unwrap(), "test · LM Studio");
        let qwen = changed(lm_studio.clone(), |value| value["host"]["modelKey"] = json!("qwen3.8-27b"));
        assert_eq!(raw_reasoning_options(&qwen).unwrap(), vec!["default", "on", "off"]);
        let direct = changed(lm_studio, |value| {
            value["profile"]["runtime"] = json!(DIRECT_RUNTIME);
            value["profile"]["model"] = json!("officestra-qwen38-27b-uncensored-q4km");
        });
        assert_eq!(raw_profile_title(&direct).unwrap(), "qwen3.8-27b-uncensored · llama.cpp · MTP");
        assert_eq!(raw_reasoning_options(&direct).unwrap(), vec!["default", "low", "medium", "xhigh"]);
        assert_eq!(raw_default_reasoning(&direct).unwrap(), "xhigh");
        assert!(raw_profile_title(&json!({ "host": {} })).is_err());
    }

    // ---- 시험 DB(OFFICE_TEST_DATABASE_URL)가 있을 때만 도는 시험 ----

    static MIGRATED: OnceCell<()> = OnceCell::const_new();

    async fn database() -> Option<PgPool> {
        let (Ok(url), Ok(migrations)) = (std::env::var("OFFICE_TEST_DATABASE_URL"), std::env::var("OFFICE_MIGRATIONS_DIR")) else {
            eprintln!("OFFICE_TEST_DATABASE_URL·OFFICE_MIGRATIONS_DIR이 없어 건너뜀");
            return None;
        };
        let pool = crate::db::connect(&url).await.unwrap();
        MIGRATED
            .get_or_init(|| async {
                crate::migrate::run(&pool, Path::new(&migrations)).await.unwrap();
            })
            .await;
        Some(pool)
    }

    async fn seed_profile(pool: &PgPool, definition: &Value, enabled: bool) {
        sqlx::query(
            "INSERT INTO local_agent_profiles(id,definition,enabled) VALUES($1,$2,$3)
             ON CONFLICT(id) DO UPDATE SET definition=EXCLUDED.definition,enabled=EXCLUDED.enabled",
        )
        .bind(definition["profile"]["id"].as_str().unwrap())
        .bind(definition)
        .bind(enabled)
        .execute(pool)
        .await
        .unwrap();
    }

    /// 직원 하나를 처음 상태(클라우드 Codex, 자동 압축 30)로 둔다. 그 직원의 세션·롤플레이 행도 지운다.
    async fn seed_character(pool: &PgPool, id: &str, config: Value) {
        for sql in [
            "DELETE FROM active_cli_sessions WHERE character_id=$1",
            "DELETE FROM cli_sessions WHERE character_id=$1",
            "DELETE FROM roleplay_profiles WHERE character_id=$1",
        ] {
            sqlx::query(sql).bind(id).execute(pool).await.unwrap();
        }
        sqlx::query(
            "INSERT INTO characters (id, name, seat, backend, model, effort, fast_mode, permission, auto_compact_percent, config)
             VALUES ($1, $1, '상단', 'codex', 'gpt-test', 'high', true, 'workspace-write', 30, $2)
             ON CONFLICT (id) DO UPDATE SET backend='codex', model='gpt-test', effort='high', fast_mode=true,
               permission='workspace-write', auto_compact_percent=30, config=EXCLUDED.config",
        )
        .bind(id)
        .bind(config)
        .execute(pool)
        .await
        .unwrap();
    }

    async fn open_session(pool: &PgPool, character_id: &str) {
        let conversation: String = sqlx::query_scalar("INSERT INTO conversations (title, workdir) VALUES ('시험', '/tmp') RETURNING id::text")
            .fetch_one(pool)
            .await
            .unwrap();
        let session: String =
            sqlx::query_scalar("INSERT INTO cli_sessions (conversation_id, character_id) VALUES ($1::uuid, $2) RETURNING id::text")
                .bind(&conversation)
                .bind(character_id)
                .fetch_one(pool)
                .await
                .unwrap();
        sqlx::query("INSERT INTO active_cli_sessions (character_id, cli_session_id) VALUES ($1, $2::uuid)")
            .bind(character_id)
            .bind(&session)
            .execute(pool)
            .await
            .unwrap();
    }

    /// (세션 수, 끝난 세션 수, 활성 세션 수).
    async fn sessions(pool: &PgPool, character_id: &str) -> (i64, i64, i64) {
        let (total, ended): (i64, i64) = sqlx::query_as("SELECT count(*), count(ended_at) FROM cli_sessions WHERE character_id=$1")
            .bind(character_id)
            .fetch_one(pool)
            .await
            .unwrap();
        let active: i64 = sqlx::query_scalar("SELECT count(*) FROM active_cli_sessions WHERE character_id=$1")
            .bind(character_id)
            .fetch_one(pool)
            .await
            .unwrap();
        (total, ended, active)
    }

    async fn stored_config(pool: &PgPool, character_id: &str) -> Value {
        sqlx::query_scalar("SELECT config FROM characters WHERE id=$1").bind(character_id).fetch_one(pool).await.unwrap()
    }

    /// (backend, model, effort, fast_mode, permission, auto_compact_percent).
    async fn stored_settings(pool: &PgPool, character_id: &str) -> (String, Option<String>, String, bool, String, i16) {
        sqlx::query_as("SELECT backend, model, effort, fast_mode, permission, auto_compact_percent FROM characters WHERE id=$1")
            .bind(character_id)
            .fetch_one(pool)
            .await
            .unwrap()
    }

    fn mark_running(runtime: &Runtime, character_id: &str) {
        let handle = TurnHandle::new(TurnState::new(TurnSeed {
            turn_id: "turn".into(),
            session_id: String::new(),
            conversation_id: String::new(),
            external_session_id: None,
            resume_execution_workdir: None,
            character: RuntimeCharacter {
                id: Some(character_id.into()),
                name: String::new(),
                seat: String::new(),
                backend: String::new(),
                model: None,
                effort: String::new(),
                fast_mode: false,
                auto_compact_percent: None,
                permission: String::new(),
                identity_prompt: String::new(),
                config: Value::Null,
                local_profile: None,
            },
            prompt: String::new(),
            execution_prompt: String::new(),
            workdir: String::new(),
            structured_result_path: None,
            attachments: Vec::new(),
            reply_recipient_ids: Vec::new(),
        }));
        runtime.jobs().running.insert(character_id.to_string(), handle);
    }

    fn preparing(runtime: &Runtime, character_id: &str) -> bool {
        runtime.jobs().preparing_characters.contains(character_id)
    }

    /// Node 시험의 localProviders 가짜. 받은 호출만 남긴다.
    #[derive(Default)]
    struct Spy {
        controls: Mutex<Vec<(Value, String, Option<String>)>>,
        closes: Mutex<Vec<(String, Option<String>)>>,
        fail: Mutex<Option<LocalError>>,
    }

    impl Spy {
        fn actions(&self) -> Vec<(String, String)> {
            self.controls
                .lock()
                .unwrap()
                .iter()
                .map(|(definition, action, _)| (definition["profile"]["id"].as_str().unwrap().to_string(), action.clone()))
                .collect()
        }
    }

    impl LocalModelControl for Spy {
        fn control_model<'a>(&'a self, raw: &'a Value, action: &'a str, comfy_release: Option<&'a str>) -> BoxFuture<'a, Result<Value, LocalError>> {
            self.controls.lock().unwrap().push((raw.clone(), action.to_string(), comfy_release.map(str::to_string)));
            let result = match self.fail.lock().unwrap().take() {
                Some(error) => Err(error),
                None if action == "start" => Ok(json!({ "state": "ready", "loaded": true })),
                None => Ok(json!({ "state": "stopped", "loaded": false })),
            };
            Box::pin(async move { result })
        }

        fn close_idle_profile<'a>(&'a self, profile_id: &'a str, verified_address: Option<&'a str>) -> BoxFuture<'a, Result<(), LocalError>> {
            self.closes.lock().unwrap().push((profile_id.to_string(), verified_address.map(str::to_string)));
            Box::pin(async { Ok(()) })
        }
    }

    fn message(result: Result<Value, LocalSelectionError>) -> String {
        result.unwrap_err().to_string()
    }

    type Events = Arc<Mutex<Vec<Value>>>;

    fn collector() -> (Events, impl Fn(Value) + Send + Sync) {
        let events = Events::default();
        let sink = Arc::clone(&events);
        (events, move |event: Value| sink.lock().unwrap().push(event))
    }

    fn event_types(events: &Events) -> Vec<String> {
        events.lock().unwrap().iter().map(|event| event["type"].as_str().unwrap_or_default().to_string()).collect()
    }

    /// Node verifyHost 가짜. 받은 호스트를 남기고, reject가 켜지면 실패한다.
    fn verifier(seen: Events, reject: Arc<AtomicBool>) -> impl Fn(LocalDefinition) -> BoxFuture<'static, Result<(), String>> + Send + Sync {
        move |definition: LocalDefinition| {
            seen.lock().unwrap().push(definition.host.to_value());
            let result = if reject.load(Ordering::SeqCst) { Err("SSH mismatch".to_string()) } else { Ok(()) };
            Box::pin(async move { result })
        }
    }

    #[tokio::test]
    async fn model_control_resolves_selected_host_and_reasoning_without_touching_assignments_or_sessions() {
        let Some(pool) = database().await else { return };
        let id = "lps-control";
        seed_profile(&pool, &definition_value(id), true).await;
        let config = json!({ "localProfileId": id, "localHostAddress": "192.0.2.15", "localReasoning": "off" });
        seed_character(&pool, id, config.clone()).await;
        open_session(&pool, id).await;
        let runtime = Runtime::new(None, None);
        let spy = Spy::default();
        let control = Some(&spy as &dyn LocalModelControl);
        let result = control_local_model(&pool, &runtime, control, id, Some("stop"), None).await.unwrap();
        assert_eq!((&result["ok"], &result["state"]), (&json!(true), &json!("stopped")));
        {
            let controls = spy.controls.lock().unwrap();
            assert_eq!(controls[0].0["host"]["address"], "192.0.2.15");
            assert_eq!(controls[0].0["profile"]["reasoning"], "off");
            assert_eq!(controls[0].1, "stop");
        }
        for action in [Some("delete"), None] {
            assert_eq!(message(control_local_model(&pool, &runtime, control, id, action, None).await), "Unsupported model action");
        }
        seed_character(&pool, "lps-control-none", json!({})).await;
        assert_eq!(
            message(control_local_model(&pool, &runtime, control, "lps-control-none", Some("stop"), None).await),
            "로컬 모델을 먼저 선택하세요."
        );
        assert_eq!(message(control_local_model(&pool, &runtime, None, id, Some("stop"), None).await), "Local provider service unavailable");
        let draining = Runtime::new(None, None);
        draining.begin_drain();
        let error = control_local_model(&pool, &draining, control, id, Some("stop"), None).await.unwrap_err();
        assert_eq!((error.to_string().as_str(), error.route_status()), (BACKEND_NOT_READY, 409));
        assert_eq!(spy.controls.lock().unwrap().len(), 1);
        assert_eq!(stored_config(&pool, id).await, config);
        assert_eq!(sessions(&pool, id).await, (1, 0, 1));
    }

    #[tokio::test]
    async fn model_control_maps_host_busy_and_gpu_occupied_to_conflicts() {
        let Some(pool) = database().await else { return };
        let id = "lps-control-busy";
        seed_profile(&pool, &definition_value(id), true).await;
        seed_character(&pool, id, json!({ "localProfileId": id })).await;
        let runtime = Runtime::new(None, None);
        let spy = Spy::default();
        let gpu = json!({ "vramPct": 93, "ramPct": 40, "comfyUI": true });
        *spy.fail.lock().unwrap() =
            Some(LocalError { status: None, message: "GPU busy".into(), busy: Some(BusyDetail::GpuOccupied(gpu.clone())) });
        let error = control_local_model(&pool, &runtime, Some(&spy), id, Some("start"), None).await.unwrap_err();
        assert_eq!((error.to_string().as_str(), error.route_status(), error.gpu()), ("GPU busy", 409, Some(&gpu)));
        *spy.fail.lock().unwrap() = Some(LocalError::busy("host busy"));
        let error = control_local_model(&pool, &runtime, Some(&spy), id, Some("start"), None).await.unwrap_err();
        assert_eq!((error.route_status(), error.gpu()), (409, None));
        *spy.fail.lock().unwrap() = Some(LocalError::plain("broken"));
        assert_eq!(control_local_model(&pool, &runtime, Some(&spy), id, Some("start"), None).await.unwrap_err().route_status(), 400);
    }

    #[tokio::test]
    async fn model_stop_also_unloads_the_roleplay_local_model_on_the_selected_host() {
        let Some(pool) = database().await else { return };
        let id = "lps-roleplay";
        let roleplay_id = "lps-roleplay-model";
        seed_profile(&pool, &definition_value(id), true).await;
        let roleplay = changed(definition_value(roleplay_id), |value| value["profile"]["model"] = json!("roleplay-test"));
        seed_profile(&pool, &roleplay, true).await;
        seed_character(&pool, id, json!({ "localProfileId": id, "localHostAddress": "192.0.2.15", "localReasoning": "off" })).await;
        sqlx::query("INSERT INTO roleplay_profiles (character_id, local_profile_id) VALUES ($1, $2)")
            .bind(id)
            .bind(roleplay_id)
            .execute(&pool)
            .await
            .unwrap();
        let runtime = Runtime::new(None, None);
        let spy = Spy::default();
        assert_eq!(control_local_model(&pool, &runtime, Some(&spy), id, Some("stop"), None).await.unwrap()["state"], "stopped");
        assert_eq!(spy.actions(), [(id.to_string(), "stop".to_string()), (roleplay_id.to_string(), "unload".to_string())]);
        {
            let controls = spy.controls.lock().unwrap();
            assert_eq!(controls[1].0["host"]["address"], "192.0.2.15");
            assert!(controls[1].0["profile"].get("reasoning").is_none_or(Value::is_null));
            assert_eq!(controls[1].2, None);
        }
        spy.controls.lock().unwrap().clear();
        sqlx::query("UPDATE roleplay_profiles SET local_profile_id=$2 WHERE character_id=$1").bind(id).bind(id).execute(&pool).await.unwrap();
        control_local_model(&pool, &runtime, Some(&spy), id, Some("stop"), None).await.unwrap();
        assert_eq!(spy.controls.lock().unwrap()[1].0["profile"]["reasoning"], "off");
        spy.controls.lock().unwrap().clear();
        control_local_model(&pool, &runtime, Some(&spy), id, Some("start"), None).await.unwrap();
        assert_eq!(spy.actions(), [(id.to_string(), "start".to_string())]);
    }

    #[tokio::test]
    async fn model_start_carries_the_chosen_comfy_release_and_rejects_it_elsewhere() {
        let Some(pool) = database().await else { return };
        let id = "lps-comfy";
        seed_profile(&pool, &definition_value(id), true).await;
        seed_character(&pool, id, json!({ "localProfileId": id })).await;
        let runtime = Runtime::new(None, None);
        let spy = Spy::default();
        let interrupt = json!("interrupt");
        control_local_model(&pool, &runtime, Some(&spy), id, Some("start"), Some(&interrupt)).await.unwrap();
        control_local_model(&pool, &runtime, Some(&spy), id, Some("start"), None).await.unwrap();
        {
            let controls = spy.controls.lock().unwrap();
            assert_eq!((controls[0].1.as_str(), controls[0].2.as_deref()), ("start", Some("interrupt")));
            assert_eq!(controls[1].2, None);
            assert_eq!(controls[0].0["host"]["address"], "192.0.2.10");
        }
        let shutdown = json!("shutdown-pc");
        for (action, release) in [("stop", &interrupt), ("start", &shutdown), ("start", &Value::Null)] {
            assert_eq!(
                message(control_local_model(&pool, &runtime, Some(&spy), id, Some(action), Some(release)).await),
                "Unsupported ComfyUI release mode"
            );
        }
        assert_eq!(spy.controls.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn address_save_verifies_pinned_host_preserves_sessions_and_rolls_back_failures() {
        let Some(pool) = database().await else { return };
        let id = "lps-address";
        seed_profile(&pool, &definition_value(id), true).await;
        seed_character(&pool, id, json!({ "localProfileId": id, "localReasoning": "on", "keep": "same" })).await;
        open_session(&pool, id).await;
        let runtime = Runtime::new(None, None);
        let spy = Spy::default();
        let (seen, reject) = (Events::default(), Arc::new(AtomicBool::new(false)));
        let verify = verifier(Arc::clone(&seen), Arc::clone(&reject));
        let verify: &VerifyHost = &verify;
        let (events, broadcast) = collector();
        let result = set_local_host_address(&pool, &runtime, Some(&spy), id, &json!(" 198.51.100.7 "), verify, &broadcast).await.unwrap();
        assert_eq!(result, json!({ "ok": true, "characterId": id, "address": ADDRESS }));
        {
            let seen = seen.lock().unwrap();
            assert_eq!(seen.len(), 1);
            assert_eq!((&seen[0]["hostKeyAlias"], &seen[0]["address"]), (&json!("test"), &json!(ADDRESS)));
        }
        assert_eq!(*spy.closes.lock().unwrap(), [(id.to_string(), Some(ADDRESS.to_string()))]);
        let saved = json!({ "localProfileId": id, "localReasoning": "on", "keep": "same", "localHostAddress": ADDRESS });
        assert_eq!(stored_config(&pool, id).await, saved);
        assert_eq!(sessions(&pool, id).await, (1, 0, 1));
        assert_eq!(event_types(&events), ["local.changed"]);
        assert!(!preparing(&runtime, id));

        reject.store(true, Ordering::SeqCst);
        assert_eq!(message(set_local_host_address(&pool, &runtime, Some(&spy), id, &json!("198.51.100.8"), verify, &broadcast).await), "SSH mismatch");
        assert_eq!(stored_config(&pool, id).await, saved);
        assert_eq!(spy.closes.lock().unwrap().len(), 1);
        assert!(!preparing(&runtime, id));
        assert_eq!(event_types(&events).len(), 1);

        reject.store(false, Ordering::SeqCst);
        assert_eq!(message(set_local_host_address(&pool, &runtime, Some(&spy), id, &json!("1.2.3.256"), verify, &broadcast).await), INVALID_ADDRESS);
        mark_running(&runtime, id);
        let busy = set_local_host_address(&pool, &runtime, Some(&spy), id, &json!("198.51.100.8"), verify, &broadcast).await.unwrap_err();
        assert!(busy.to_string().contains("작업") && busy.to_string().contains("터미널"));
        assert_eq!(busy.route_status(), 409);
        runtime.jobs().running.clear();
        runtime.jobs().compacting.insert(id.to_string());
        assert_eq!(message(set_local_host_address(&pool, &runtime, Some(&spy), id, &json!("198.51.100.8"), verify, &broadcast).await), LOCAL_SELECTION_BUSY);
        runtime.jobs().compacting.clear();
        runtime.jobs().preparing_characters.insert(id.to_string());
        assert_eq!(message(set_local_host_address(&pool, &runtime, Some(&spy), id, &json!("198.51.100.8"), verify, &broadcast).await), LOCAL_SELECTION_BUSY);
        // 남이 등록한 준비 표시는 지우지 않는다.
        assert!(preparing(&runtime, id));
        runtime.jobs().preparing_characters.clear();
        assert_eq!(seen.lock().unwrap().len(), 2);
        assert_eq!(stored_config(&pool, id).await, saved);

        seed_character(&pool, "lps-address-none", json!({ "keep": "same" })).await;
        assert_eq!(
            message(set_local_host_address(&pool, &runtime, Some(&spy), "lps-address-none", &json!(ADDRESS), verify, &broadcast).await),
            "Local profile required"
        );
    }

    #[tokio::test]
    async fn reasoning_change_is_idle_only_and_preserves_existing_settings_and_sessions() {
        let Some(pool) = database().await else { return };
        let id = "lps-reasoning";
        let definition = changed(definition_value(id), |value| value["host"]["modelKey"] = json!("qwen3.8-27b"));
        seed_profile(&pool, &definition, true).await;
        seed_character(&pool, id, json!({ "localProfileId": id, "keep": "unchanged" })).await;
        open_session(&pool, id).await;
        let runtime = Runtime::new(None, None);
        let (events, broadcast) = collector();
        assert_eq!(set_local_reasoning(&pool, &runtime, id, &json!("on"), &broadcast).await.unwrap()["reasoning"], "on");
        assert_eq!(stored_config(&pool, id).await, json!({ "localProfileId": id, "keep": "unchanged", "localReasoning": "on" }));
        assert_eq!(sessions(&pool, id).await, (1, 0, 1));
        assert_eq!(event_types(&events), ["local.changed"]);
        assert!(!preparing(&runtime, id));
        mark_running(&runtime, id);
        let busy = set_local_reasoning(&pool, &runtime, id, &json!("off"), &broadcast).await.unwrap_err();
        assert!(busy.to_string().contains("작업"));
        assert_eq!(busy.generic_status(), 409);
        runtime.jobs().running.clear();
        let unsupported = set_local_reasoning(&pool, &runtime, id, &json!("xhigh"), &broadcast).await.unwrap_err();
        assert_eq!(unsupported.to_string(), "Unsupported local reasoning option for this runtime");
        assert_eq!(unsupported.generic_status(), 500);
        assert_eq!(message(set_local_reasoning(&pool, &runtime, id, &json!("max"), &broadcast).await), "Unsupported local reasoning option");
        assert_eq!(stored_config(&pool, id).await["localReasoning"], "on");
        assert!(!preparing(&runtime, id));
    }

    #[tokio::test]
    async fn direct_runtime_persists_each_verified_effort_without_replacing_the_session() {
        let Some(pool) = database().await else { return };
        let id = "lps-direct";
        let direct = changed(definition_value(id), |value| {
            let profile = &mut value["profile"];
            profile["backend"] = json!("codex");
            profile["runtime"] = json!(DIRECT_RUNTIME);
            profile["model"] = json!("officestra-qwen38-27b-uncensored-q4km");
            profile["contextWindow"] = json!(65536);
            profile["kvCacheQuantization"] = json!("q8_0");
            profile["usageProtocol"] = json!("openai-responses-v1");
            value["host"]["modelKey"] = json!("qwen3.8-27b");
        });
        seed_profile(&pool, &direct, true).await;
        seed_character(&pool, id, json!({ "localProfileId": id, "keep": "unchanged" })).await;
        open_session(&pool, id).await;
        let runtime = Runtime::new(None, None);
        let (_, broadcast) = collector();
        for reasoning in ["low", "medium", "xhigh"] {
            assert_eq!(set_local_reasoning(&pool, &runtime, id, &json!(reasoning), &broadcast).await.unwrap()["reasoning"], reasoning);
            assert_eq!(stored_config(&pool, id).await["localReasoning"], reasoning);
        }
        assert_eq!(sessions(&pool, id).await, (1, 0, 1));
        assert!(message(set_local_reasoning(&pool, &runtime, id, &json!("on"), &broadcast).await).starts_with("Unsupported"));
    }

    #[tokio::test]
    async fn select_and_restore_end_sessions_without_deleting_history() {
        let Some(pool) = database().await else { return };
        let id = "lps-select";
        let second = "lps-select-second";
        seed_profile(&pool, &definition_value(id), true).await;
        seed_profile(&pool, &changed(definition_value(second), |value| value["profile"]["model"] = json!("other")), true).await;
        let cloud = json!({ "executablePath": "/custom/codex", "keep": "unchanged" });
        seed_character(&pool, id, cloud.clone()).await;
        open_session(&pool, id).await;
        let runtime = Runtime::new(None, None);
        let spy = Spy::default();
        let (events, broadcast) = collector();

        let result = select_local_profile(&pool, &runtime, Some(&spy), id, Some(id), &broadcast).await.unwrap();
        assert_eq!(result, json!({ "ok": true, "characterId": id, "localProfileId": id, "newSession": true }));
        assert_eq!(
            stored_settings(&pool, id).await,
            ("claude".into(), Some("qwen-test".into()), "default".into(), false, "auto".into(), 100)
        );
        let config = stored_config(&pool, id).await;
        assert_eq!(config["localProfileId"], id);
        assert!(config.get("executablePath").is_none());
        assert_eq!(sessions(&pool, id).await, (1, 1, 0));
        assert!(spy.closes.lock().unwrap().is_empty());
        assert_eq!(event_types(&events), ["local.changed", "session.changed"]);
        assert!(!preparing(&runtime, id));

        // 같은 프로필을 다시 고르면 세션을 건드리지 않는다.
        open_session(&pool, id).await;
        let same = select_local_profile(&pool, &runtime, Some(&spy), id, Some(id), &broadcast).await.unwrap();
        assert_eq!(same["newSession"], false);
        assert_eq!(sessions(&pool, id).await, (2, 1, 1));

        // 다른 로컬 프로필로 바꾸면 이전 프로필의 유휴 연결을 닫고, 복원 지점은 그대로 둔다.
        select_local_profile(&pool, &runtime, Some(&spy), id, Some(second), &broadcast).await.unwrap();
        assert_eq!(*spy.closes.lock().unwrap(), [(id.to_string(), None)]);
        assert_eq!(stored_config(&pool, id).await["localPreviousSettings"], config["localPreviousSettings"]);
        assert_eq!(stored_settings(&pool, id).await.1.as_deref(), Some("other"));
        assert_eq!(sessions(&pool, id).await, (2, 2, 0));

        let restored = select_local_profile(&pool, &runtime, Some(&spy), id, None, &broadcast).await.unwrap();
        assert_eq!((&restored["localProfileId"], &restored["newSession"]), (&Value::Null, &json!(true)));
        assert_eq!(
            stored_settings(&pool, id).await,
            ("codex".into(), Some("gpt-test".into()), "high".into(), true, "workspace-write".into(), 30)
        );
        assert_eq!(stored_config(&pool, id).await, cloud);
        assert_eq!(spy.closes.lock().unwrap().last(), Some(&(second.to_string(), None)));
        assert_eq!(sessions(&pool, id).await.0, 2);
        // 이미 클라우드면 바꿀 것이 없다. 빈 문자열은 복원 요청이다.
        assert_eq!(select_local_profile(&pool, &runtime, Some(&spy), id, Some(""), &broadcast).await.unwrap()["newSession"], false);
        assert_eq!(
            message(select_local_profile(&pool, &runtime, Some(&spy), id, Some("lps-select-missing"), &broadcast).await),
            "사용 가능한 로컬 모델이 없습니다."
        );
        assert!(!preparing(&runtime, id));
    }

    #[tokio::test]
    async fn failed_restore_write_rolls_back_and_releases_the_selection_gate() {
        let Some(pool) = database().await else { return };
        let id = "lps-select-fail";
        // 백업에 backend가 없으면 복원 UPDATE가 NOT NULL에 걸린다(Node 시험의 'write failed' 대신).
        let config = json!({ "localProfileId": id, "localPreviousSettings": { "model": "gpt-test" } });
        seed_character(&pool, id, config.clone()).await;
        open_session(&pool, id).await;
        let runtime = Runtime::new(None, None);
        let (events, broadcast) = collector();
        let error = select_local_profile(&pool, &runtime, None, id, None, &broadcast).await.unwrap_err();
        assert!(matches!(error, LocalSelectionError::Database(_)));
        assert_eq!(error.generic_status(), 500);
        assert_eq!(stored_config(&pool, id).await, config);
        assert_eq!(sessions(&pool, id).await, (1, 0, 1));
        assert!(events.lock().unwrap().is_empty());
        assert!(!preparing(&runtime, id));
        let unavailable = json!({ "localProfileId": id });
        seed_character(&pool, id, unavailable).await;
        assert_eq!(message(select_local_profile(&pool, &runtime, None, id, None, &broadcast).await), "Previous cloud settings are unavailable");
    }

    #[tokio::test]
    async fn running_compacting_and_draining_employees_are_blocked_without_touching_settings() {
        let Some(pool) = database().await else { return };
        let id = "lps-select-busy";
        seed_profile(&pool, &definition_value(id), true).await;
        seed_character(&pool, id, json!({})).await;
        open_session(&pool, id).await;
        let runtime = Runtime::new(None, None);
        let (events, broadcast) = collector();
        mark_running(&runtime, id);
        let error = select_local_profile(&pool, &runtime, None, id, Some(id), &broadcast).await.unwrap_err();
        assert_eq!((error.to_string().as_str(), error.generic_status()), (LOCAL_SELECTION_BUSY, 409));
        runtime.jobs().running.clear();
        runtime.jobs().compacting.insert(id.to_string());
        assert_eq!(message(select_local_profile(&pool, &runtime, None, id, Some(id), &broadcast).await), LOCAL_SELECTION_BUSY);
        runtime.jobs().compacting.clear();
        runtime.begin_drain();
        assert_eq!(message(select_local_profile(&pool, &runtime, None, id, Some(id), &broadcast).await), BACKEND_NOT_READY);
        assert_eq!(stored_settings(&pool, id).await.0, "codex");
        assert_eq!(stored_config(&pool, id).await, json!({}));
        assert_eq!(sessions(&pool, id).await, (1, 0, 1));
        assert!(events.lock().unwrap().is_empty());
        // 다른 직원은 막히지 않는다.
        let other = "lps-select-busy-other";
        seed_character(&pool, other, json!({})).await;
        runtime.cancel_drain();
        mark_running(&runtime, id);
        assert_eq!(select_local_profile(&pool, &runtime, None, other, Some(id), &broadcast).await.unwrap()["newSession"], true);
    }

    #[tokio::test]
    async fn profile_listing_uses_raw_definitions_and_saving_normalizes_them() {
        let Some(pool) = database().await else { return };
        let id = "lps-list";
        let legacy = "lps-list-legacy";
        seed_profile(&pool, &definition_value(id), true).await;
        seed_profile(&pool, &changed(definition_value(legacy), |value| value["profile"]["legacy"] = json!(true)), false).await;
        seed_character(&pool, id, json!({ "localProfileId": id, "localReasoning": "off" })).await;
        let directory = std::env::temp_dir().join(format!("lps-list-{}", std::process::id()));
        let service = LocalProviderService::new(crate::local_provider::service::ServiceOptions::new(
            pool.clone(),
            directory.clone(),
            Arc::new(|_| {}),
        ))
        .unwrap();
        let listing = list_local_profiles(&pool, Some(&service)).await.unwrap();
        let profile = |profile_id: &str| listing["profiles"].as_array().unwrap().iter().find(|profile| profile["id"] == profile_id).cloned().unwrap();
        assert_eq!(
            profile(id),
            json!({
                "id": id, "enabled": true, "backend": "claude", "model": "qwen-test", "title": "test · LM Studio", "contextWindow": 32768,
                "kvCacheQuantization": null, "reasoningOptions": [], "defaultReasoning": "default",
            })
        );
        assert_eq!((&profile(legacy)["enabled"], &profile(legacy)["title"]), (&json!(false), &json!("test · LM Studio")));
        let assignment = listing["assignments"].as_array().unwrap().iter().find(|row| row["characterId"] == id).cloned().unwrap();
        assert_eq!((&assignment["profileId"], &assignment["reasoning"]), (&json!(id), &json!("off")));
        let status = listing["statuses"].as_array().unwrap().iter().find(|row| row["characterId"] == id).cloned().unwrap();
        assert_eq!((&status["id"], &status["profileId"], &status["state"], &status["loaded"]), (&json!(id), &json!(id), &json!("idle"), &json!(false)));
        assert!(list_local_profiles(&pool, None).await.unwrap()["statuses"].as_array().unwrap().is_empty());

        let saved = "lps-save";
        assert_eq!(save_local_profile(&pool, Some(&service), &json!({ "definition": definition_value(saved) })).await.unwrap(), json!({ "id": saved }));
        let (definition, enabled): (Value, bool) = sqlx::query_as("SELECT definition, enabled FROM local_agent_profiles WHERE id=$1")
            .bind(saved)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!((definition, enabled), (definition_of(&definition_value(saved)).to_value(), false));
        save_local_profile(&pool, None, &json!({ "definition": definition_value(saved), "enabled": true })).await.unwrap();
        let enabled: bool = sqlx::query_scalar("SELECT enabled FROM local_agent_profiles WHERE id=$1").bind(saved).fetch_one(&pool).await.unwrap();
        assert!(enabled);
        let rejected = changed(definition_value(saved), |value| value["profile"]["legacy"] = json!(true));
        assert_eq!(save_local_profile(&pool, None, &json!({ "definition": rejected })).await.unwrap_err().0, 500);
        assert_eq!(save_local_profile(&pool, None, &json!({})).await.unwrap_err().0, 500);
        let _ = std::fs::remove_dir_all(directory);
    }
}
