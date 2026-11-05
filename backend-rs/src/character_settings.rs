// 직원 CLI 설정의 검증과 원자적 전환. Node의 character-settings.mjs와 server.mjs의 listCharacters·updateCharacterSettings를 옮겼다.

use std::fmt;

use serde::Serialize;
use serde_json::{Map, Value};
use sqlx::{Connection, PgConnection, PgPool};

use crate::agent_provider::{
    AGENT_BACKENDS, backend_efforts, backend_models, backend_permissions, backend_supports_fast_mode,
    normalized_backend_permission,
};
use crate::antigravity_model_selection::is_antigravity_fixed_thinking_model;
use crate::configuration::{SessionTarget, character_settings_require_new_session};
use crate::git_workspace::GitWorkspaceError;
use crate::health::{Runtime, SessionEndPlan};
use crate::model_catalog::ModelCatalogService;

pub const MAX_BULK_CHARACTER_SETTINGS: usize = 100;

#[derive(Debug)]
pub enum CharacterSettingsError {
    /// 400. Node의 CharacterSettingsValidationError.
    Validation(String),
    /// 404 + characterIds. Node의 CharacterSettingsTargetsNotFoundError.
    TargetsNotFound(Vec<String>),
    /// 404. 개별 설정 PUT에서 직원이 없을 때.
    CharacterNotFound,
    /// 409. Node의 CharacterSettingsDrainConflictError.
    DrainConflict(String),
    /// 409. Node의 AgentBusyError.
    Busy(String),
    /// 409. Node의 GitWorkspaceError 등 작업 공간 정리 실패.
    Workspace(String),
    /// 500. 작업 공간 확인 중 fs 오류처럼 Node가 그대로 올리는 오류.
    Failed(String),
    /// 500.
    Database(sqlx::Error),
}

impl fmt::Display for CharacterSettingsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Validation(m) | Self::DrainConflict(m) | Self::Busy(m) | Self::Workspace(m) | Self::Failed(m) => {
                f.write_str(m)
            }
            Self::TargetsNotFound(ids) => write!(f, "캐릭터를 찾을 수 없습니다: {}", ids.join(", ")),
            Self::CharacterNotFound => f.write_str("캐릭터를 찾을 수 없습니다."),
            Self::Database(error) => write!(f, "{error}"),
        }
    }
}

impl From<sqlx::Error> for CharacterSettingsError {
    fn from(error: sqlx::Error) -> Self {
        Self::Database(error)
    }
}

impl From<GitWorkspaceError> for CharacterSettingsError {
    fn from(error: GitWorkspaceError) -> Self {
        if error.is_git_workspace_error() { Self::Workspace(error.message) } else { Self::Failed(error.message) }
    }
}

/// 동적 모델 카탈로그(R09)가 아는 모델의 능력. 아직 카탈로그가 없으면 내장 목록만 쓴다.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelCapabilities {
    pub efforts: Vec<String>,
    pub supports_fast_mode: bool,
}

pub trait ModelCatalog: Sync {
    fn model_capabilities(&self, backend: &str, model: &str) -> Option<ModelCapabilities>;
}

/// Node의 modelCatalogService처럼 실행 중 카탈로그로 설정을 검사한다. 카탈로그를 못 읽으면 내장 목록으로 넘어간다.
impl ModelCatalog for ModelCatalogService {
    fn model_capabilities(&self, backend: &str, model: &str) -> Option<ModelCapabilities> {
        let entry = ModelCatalogService::model_capabilities(self, backend, model).ok()??;
        Some(ModelCapabilities { efforts: entry.efforts, supports_fast_mode: entry.supports_fast_mode })
    }
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CharacterSettingsUpdate {
    #[serde(rename = "characterID")]
    pub character_id: String,
    pub backend: String,
    pub model: String,
    pub effort: String,
    pub fast_mode: bool,
    pub permission: String,
}

/// Node의 String(value ?? "")과 같다. 문자열은 그대로, null·없음은 빈 문자열, 그 밖은 JSON 표기.
fn string_of(value: Option<&Value>) -> String {
    match value {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(text)) => text.clone(),
        Some(other) => other.to_string(),
    }
}

pub fn normalize_character_settings_update(
    body: &Value,
    require_character_id: bool,
    catalog: Option<&dyn ModelCatalog>,
) -> Result<CharacterSettingsUpdate, CharacterSettingsError> {
    use CharacterSettingsError::Validation;
    let Some(object) = body.as_object() else {
        return Err(Validation("직원 설정은 JSON 객체여야 합니다.".to_string()));
    };
    let character_id = string_of(object.get("characterId")).trim().to_string();
    if require_character_id && character_id.is_empty() {
        return Err(Validation("직원 식별자를 입력하세요.".to_string()));
    }
    let backend = string_of(object.get("backend"));
    if !AGENT_BACKENDS.contains(&backend.as_str()) {
        return Err(Validation("지원하지 않는 CLI입니다.".to_string()));
    }
    let model = string_of(object.get("model"));
    let dynamic = catalog.and_then(|catalog| catalog.model_capabilities(&backend, &model));
    if dynamic.is_none() && !backend_models(&backend).contains(&model.as_str()) {
        return Err(Validation("지원하지 않는 모델입니다.".to_string()));
    }
    let requested_effort = string_of(object.get("effort"));
    let effort = if backend == "antigravity" && is_antigravity_fixed_thinking_model(&model) && requested_effort == "high" {
        "default".to_string()
    } else {
        requested_effort
    };
    let supported_efforts: Vec<&str> = match &dynamic {
        Some(capabilities) if !capabilities.efforts.is_empty() => capabilities.efforts.iter().map(String::as_str).collect(),
        _ => backend_efforts(&backend, Some(&model)).to_vec(),
    };
    if !supported_efforts.contains(&effort.as_str()) {
        return Err(Validation("지원하지 않는 추론 레벨입니다.".to_string()));
    }
    let Some(fast_mode) = object.get("fastMode").and_then(Value::as_bool) else {
        return Err(Validation("Fast 모드 설정은 참 또는 거짓이어야 합니다.".to_string()));
    };
    let supports_fast_mode = match &dynamic {
        Some(capabilities) => capabilities.supports_fast_mode,
        None => backend_supports_fast_mode(&backend, &model),
    };
    if fast_mode && !supports_fast_mode {
        return Err(Validation("선택한 CLI와 모델은 Fast 모드를 지원하지 않습니다.".to_string()));
    }
    let requested_permission = string_of(object.get("permission"));
    if !backend_permissions(&backend).contains(&requested_permission.as_str()) {
        return Err(Validation("지원하지 않는 권한입니다.".to_string()));
    }
    Ok(CharacterSettingsUpdate {
        character_id,
        permission: normalized_backend_permission(&backend, &requested_permission),
        backend,
        model,
        effort,
        fast_mode,
    })
}

pub fn normalize_bulk_character_settings(
    body: &Value,
    catalog: Option<&dyn ModelCatalog>,
) -> Result<Vec<CharacterSettingsUpdate>, CharacterSettingsError> {
    use CharacterSettingsError::Validation;
    let Some(updates) = body.get("updates").and_then(Value::as_array) else {
        return Err(Validation("updates 배열을 입력하세요.".to_string()));
    };
    if updates.is_empty() || updates.len() > MAX_BULK_CHARACTER_SETTINGS {
        return Err(Validation(format!("직원 설정은 1개 이상 {MAX_BULK_CHARACTER_SETTINGS}개 이하여야 합니다.")));
    }
    let updates = updates
        .iter()
        .map(|update| normalize_character_settings_update(update, true, catalog))
        .collect::<Result<Vec<_>, _>>()?;
    let mut seen = std::collections::HashSet::new();
    for update in &updates {
        if !seen.insert(update.character_id.as_str()) {
            return Err(Validation(format!("같은 직원 설정을 두 번 보낼 수 없습니다: {}", update.character_id)));
        }
    }
    Ok(updates)
}

pub fn sorted_unique_character_ids(character_ids: &[String]) -> Vec<&str> {
    let mut sorted: Vec<&str> = character_ids.iter().map(String::as_str).collect();
    sorted.sort_unstable();
    sorted.dedup();
    sorted
}

/// 직원별 advisory lock을 정렬 순서로 잡고 본문을 실행한 뒤 역순으로 푼다. 연결 하나를 끝까지 쓴다.
pub async fn with_character_session_locks<T, E: From<sqlx::Error>>(
    pool: &PgPool,
    character_ids: &[String],
    body: impl AsyncFnOnce(&mut PgConnection) -> Result<T, E>,
) -> Result<T, E> {
    let sorted = sorted_unique_character_ids(character_ids);
    let mut connection = pool.acquire().await?;
    let mut locked: Vec<&str> = Vec::new();
    let mut lock_error = None;
    for character_id in &sorted {
        let lock = sqlx::query("SELECT pg_advisory_lock(hashtext($1))")
            .bind(format!("officestra:character:{character_id}"))
            .execute(&mut *connection)
            .await;
        if let Err(error) = lock {
            lock_error = Some(error);
            break;
        }
        locked.push(character_id);
    }
    let outcome = match lock_error {
        Some(error) => Err(E::from(error)),
        None => body(&mut connection).await,
    };
    for character_id in locked.iter().rev() {
        let unlock = sqlx::query("SELECT pg_advisory_unlock(hashtext($1))")
            .bind(format!("officestra:character:{character_id}"))
            .execute(&mut *connection)
            .await;
        if let Err(error) = unlock
            && outcome.is_ok()
        {
            eprintln!("직원 설정 잠금을 해제하지 못했습니다. {error}");
        }
    }
    outcome
}

#[derive(Debug, Clone, Serialize, sqlx::FromRow, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct CurrentCharacter {
    pub id: String,
    pub name: String,
    pub backend: String,
    pub model: Option<String>,
    pub effort: String,
    pub fast_mode: bool,
    pub permission: String,
    pub config: Value,
    pub identity_prompt: String,
}

#[derive(Debug, Clone, Serialize, sqlx::FromRow, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct UpdatedCharacter {
    pub id: String,
    pub name: String,
    pub backend: String,
    pub model: Option<String>,
    pub effort: String,
    pub fast_mode: bool,
    pub permission: String,
    pub identity_prompt: String,
}

/// 바뀐 직원은 config 없이, 그대로인 직원은 현재 행 그대로 돌려준다. Node와 같은 모양이다.
#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(untagged)]
pub enum BulkCharacter {
    Current(CurrentCharacter),
    Updated(UpdatedCharacter),
}

impl BulkCharacter {
    pub fn id(&self) -> &str {
        match self {
            Self::Current(character) => &character.id,
            Self::Updated(character) => &character.id,
        }
    }
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct BulkUpdateResult {
    pub ok: bool,
    pub characters: Vec<BulkCharacter>,
    pub warnings: Vec<String>,
}

fn is_truthy(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) => false,
        Some(Value::Bool(flag)) => *flag,
        Some(Value::String(text)) => !text.is_empty(),
        Some(Value::Number(number)) => number.as_f64().is_some_and(|n| n != 0.0),
        Some(_) => true,
    }
}

fn local_profile_busy(config: &Value) -> Result<(), CharacterSettingsError> {
    if is_truthy(config.get("localProfileId")) {
        return Err(CharacterSettingsError::Busy("로컬 4090 메뉴에서 클라우드로 돌아간 뒤 설정을 변경하세요.".to_string()));
    }
    Ok(())
}

/// CLI가 바뀌면 옛 실행 경로를 config에서 뺀다. sqlx 0.9는 동적 SQL 문자열을 거부하므로 리터럴로 잇는다.
macro_rules! update_character_settings_sql {
    ($returning:literal) => {
        concat!(
            "UPDATE characters
             SET backend = $2, model = $3, effort = $4, fast_mode = $5, permission = $6,
                 config = CASE WHEN characters.backend <> $2 THEN COALESCE(characters.config, '{}'::jsonb) - 'executablePath' ELSE characters.config END,
                 updated_at = now()
             WHERE id = $1 RETURNING ",
            $returning
        )
    };
}

/// 여러 직원의 설정을 한 트랜잭션에서 바꾼다. 검증 → 드레인 → 잠금 → 대상 확인 → 세션 종료 사전 검사 → 갱신 순서다.
pub async fn update_character_settings_atomically(
    pool: &PgPool,
    runtime: &Runtime,
    body: &Value,
    catalog: Option<&dyn ModelCatalog>,
) -> Result<BulkUpdateResult, CharacterSettingsError> {
    let updates = normalize_bulk_character_settings(body, catalog)?;
    if runtime.is_draining() {
        return Err(CharacterSettingsError::DrainConflict("백엔드가 이미 다른 안전 전환을 준비 중입니다.".to_string()));
    }
    runtime.begin_drain();
    let result = update_while_draining(pool, runtime, &updates).await;
    runtime.cancel_drain();
    result
}

async fn update_while_draining(
    pool: &PgPool,
    runtime: &Runtime,
    updates: &[CharacterSettingsUpdate],
) -> Result<BulkUpdateResult, CharacterSettingsError> {
    if !runtime.maintenance_status().idle {
        return Err(CharacterSettingsError::Busy(
            "실행 중인 업무와 승인·병합 후속 처리가 끝난 뒤 설정을 변경하세요.".to_string(),
        ));
    }
    let character_ids: Vec<String> = updates.iter().map(|update| update.character_id.clone()).collect();
    with_character_session_locks(pool, &character_ids, async |connection: &mut PgConnection| {
        let mut transaction = connection.begin().await?;
        match apply_bulk_updates(&mut *transaction, runtime, updates, &character_ids).await {
            Ok((characters, plans)) => {
                transaction.commit().await?;
                let mut warnings = Vec::new();
                for plan in &plans {
                    match runtime.finalize_workspace_session_end_plan(plan).await {
                        Ok(Some(warning)) => warnings.push(warning),
                        Ok(None) => {}
                        Err(error) => warnings.push(format!("{}: {error}", plan.character_id)),
                    }
                }
                Ok(BulkUpdateResult { ok: true, characters, warnings })
            }
            Err(error) => {
                transaction.rollback().await.ok();
                Err(error)
            }
        }
    })
    .await
}

async fn apply_bulk_updates(
    connection: &mut PgConnection,
    runtime: &Runtime,
    updates: &[CharacterSettingsUpdate],
    character_ids: &[String],
) -> Result<(Vec<BulkCharacter>, Vec<SessionEndPlan>), CharacterSettingsError> {
    let current: Vec<CurrentCharacter> = sqlx::query_as(
        "SELECT id, name, backend, model, effort, fast_mode, permission, config, identity_prompt
         FROM characters WHERE id = ANY($1::text[]) ORDER BY id FOR UPDATE",
    )
    .bind(character_ids)
    .fetch_all(&mut *connection)
    .await?;
    let missing: Vec<String> = character_ids
        .iter()
        .filter(|id| !current.iter().any(|character| &character.id == *id))
        .cloned()
        .collect();
    if !missing.is_empty() {
        return Err(CharacterSettingsError::TargetsNotFound(missing));
    }
    let previous_of = |id: &str| current.iter().find(|character| character.id == id).expect("대상 확인 뒤 호출");

    let mut plans = Vec::new();
    for update in updates {
        let previous = previous_of(&update.character_id);
        local_profile_busy(&previous.config)?;
        let previous_target = SessionTarget {
            backend: &previous.backend,
            local_profile_id: previous.config.get("localProfileId").and_then(Value::as_str),
        };
        let next_target = SessionTarget { backend: &update.backend, local_profile_id: None };
        if character_settings_require_new_session(&previous_target, &next_target) {
            let plan = runtime.inspect_workspace_for_session_end(&update.character_id, connection).await?;
            if plan.has_review_changes() {
                return Err(CharacterSettingsError::Busy(
                    "CLI를 변경하기 전에 현재 작업 공간의 변경사항을 승인하거나 거절하세요.".to_string(),
                ));
            }
            plans.push(plan);
        }
    }
    for plan in &plans {
        runtime.apply_workspace_session_end_plan(connection, plan).await?;
    }

    let mut characters = Vec::with_capacity(updates.len());
    for update in updates {
        let previous = previous_of(&update.character_id);
        let changed = previous.backend != update.backend
            || previous.model.as_deref() != Some(update.model.as_str())
            || previous.effort != update.effort
            || previous.fast_mode != update.fast_mode
            || previous.permission != update.permission;
        if !changed {
            characters.push(BulkCharacter::Current(previous.clone()));
            continue;
        }
        let updated: Option<UpdatedCharacter> = sqlx::query_as(update_character_settings_sql!(
            "id, name, backend, model, effort, fast_mode, permission, identity_prompt"
        ))
        .bind(&update.character_id)
        .bind(&update.backend)
        .bind(&update.model)
        .bind(&update.effort)
        .bind(update.fast_mode)
        .bind(&update.permission)
        .fetch_optional(&mut *connection)
        .await?;
        match updated {
            Some(row) => characters.push(BulkCharacter::Updated(row)),
            None => return Err(CharacterSettingsError::TargetsNotFound(vec![update.character_id.clone()])),
        }
    }
    Ok((characters, plans))
}

#[derive(Debug, Clone, Serialize, sqlx::FromRow, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct SingleCurrentCharacter {
    pub id: String,
    pub backend: String,
    pub model: Option<String>,
    pub effort: String,
    pub fast_mode: bool,
    pub permission: String,
    pub config: Value,
}

#[derive(Debug, Clone, Serialize, sqlx::FromRow, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct SingleUpdatedCharacter {
    pub id: String,
    pub backend: String,
    pub model: Option<String>,
    pub effort: String,
    pub fast_mode: bool,
    pub permission: String,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(untagged)]
pub enum SingleCharacter {
    Current(SingleCurrentCharacter),
    Updated(SingleUpdatedCharacter),
}

/// PUT /api/characters/{id}/settings. 경로의 직원 ID가 본문의 characterId를 덮어쓴다.
pub async fn update_single_character_settings(
    pool: &PgPool,
    runtime: &Runtime,
    character_id: &str,
    body: &Value,
    catalog: Option<&dyn ModelCatalog>,
) -> Result<SingleCharacter, CharacterSettingsError> {
    let mut object: Map<String, Value> = body.as_object().cloned().unwrap_or_default();
    object.insert("characterId".to_string(), Value::String(character_id.to_string()));
    let update = normalize_character_settings_update(&Value::Object(object), true, catalog)?;
    let character_ids = [character_id.to_string()];
    with_character_session_locks(pool, &character_ids, async |connection: &mut PgConnection| {
        let mut transaction = connection.begin().await?;
        match apply_single_update(&mut *transaction, runtime, character_id, &update).await {
            Ok(character) => {
                transaction.commit().await?;
                Ok(character)
            }
            Err(error) => {
                transaction.rollback().await.ok();
                Err(error)
            }
        }
    })
    .await
}

async fn apply_single_update(
    connection: &mut PgConnection,
    runtime: &Runtime,
    character_id: &str,
    update: &CharacterSettingsUpdate,
) -> Result<SingleCharacter, CharacterSettingsError> {
    let previous: Option<SingleCurrentCharacter> = sqlx::query_as(
        "SELECT id, backend, model, effort, fast_mode, permission, config FROM characters WHERE id = $1 FOR UPDATE",
    )
    .bind(character_id)
    .fetch_optional(&mut *connection)
    .await?;
    let Some(previous) = previous else {
        return Err(CharacterSettingsError::CharacterNotFound);
    };
    local_profile_busy(&previous.config)?;
    let previous_target = SessionTarget {
        backend: &previous.backend,
        local_profile_id: previous.config.get("localProfileId").and_then(Value::as_str),
    };
    let requires_new_session = character_settings_require_new_session(
        &previous_target,
        &SessionTarget { backend: &update.backend, local_profile_id: None },
    );
    let changed = previous.backend != update.backend
        || previous.model.as_deref() != Some(update.model.as_str())
        || previous.effort != update.effort
        || previous.fast_mode != update.fast_mode
        || previous.permission != update.permission;
    if !changed {
        return Ok(SingleCharacter::Current(previous));
    }
    if requires_new_session {
        runtime.prepare_workspace_for_session_end(character_id).await?;
    }
    let updated: SingleUpdatedCharacter = sqlx::query_as(update_character_settings_sql!(
        "id, backend, model, effort, fast_mode, permission"
    ))
    .bind(character_id)
    .bind(&update.backend)
    .bind(&update.model)
    .bind(&update.effort)
    .bind(update.fast_mode)
    .bind(&update.permission)
    .fetch_one(&mut *connection)
    .await?;
    Ok(SingleCharacter::Updated(updated))
}

#[derive(Debug, Clone, Serialize, sqlx::FromRow, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ListedCharacter {
    pub id: String,
    pub name: String,
    pub seat: String,
    pub backend: String,
    pub model: Option<String>,
    pub effort: String,
    pub fast_mode: bool,
    pub auto_compact_percent: i16,
    pub permission: String,
    pub identity_prompt: String,
}

/// GET /api/characters. 좌석 순서(boss, left-man, left-woman, right-woman, right-man)로 정렬한다.
pub async fn list_characters(pool: &PgPool) -> Result<Vec<ListedCharacter>, sqlx::Error> {
    sqlx::query_as(
        "SELECT id, name, seat, backend, model, effort, fast_mode, auto_compact_percent, permission, identity_prompt
         FROM characters
         ORDER BY CASE id WHEN 'boss' THEN 1 WHEN 'left-man' THEN 2 WHEN 'left-woman' THEN 3
                          WHEN 'right-woman' THEN 4 WHEN 'right-man' THEN 5 ELSE 99 END",
    )
    .fetch_all(pool)
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn settings(character_id: &str, patch: Value) -> Value {
        let mut object = json!({
            "characterId": character_id, "backend": "codex", "model": "gpt-5.6-sol",
            "effort": "high", "fastMode": false, "permission": "workspace-write"
        });
        object.as_object_mut().unwrap().extend(patch.as_object().unwrap().clone());
        object
    }

    fn bulk(updates: Vec<Value>, catalog: Option<&dyn ModelCatalog>) -> Result<Vec<CharacterSettingsUpdate>, String> {
        normalize_bulk_character_settings(&json!({ "updates": updates }), catalog).map_err(|error| error.to_string())
    }

    struct FixedCatalog(Vec<(&'static str, &'static str, ModelCapabilities)>);
    impl ModelCatalog for FixedCatalog {
        fn model_capabilities(&self, backend: &str, model: &str) -> Option<ModelCapabilities> {
            self.0.iter().find(|(b, m, _)| *b == backend && *m == model).map(|(_, _, c)| c.clone())
        }
    }

    #[test]
    fn 고정_thinking_모델은_high를_default로_저장한다() {
        let catalog = FixedCatalog(vec![(
            "antigravity",
            "claude-opus-4-6-thinking",
            ModelCapabilities { efforts: vec!["default".into()], supports_fast_mode: false },
        )]);
        for effort in ["default", "high"] {
            let normalized = bulk(
                vec![settings("right-man", json!({ "backend": "antigravity", "model": "claude-opus-4-6-thinking", "effort": effort, "permission": "plan" }))],
                Some(&catalog),
            )
            .unwrap();
            assert_eq!(normalized[0].effort, "default");
        }
        let error = bulk(
            vec![settings("right-man", json!({ "backend": "antigravity", "model": "claude-opus-4-6-thinking", "effort": "low", "permission": "plan" }))],
            Some(&catalog),
        )
        .unwrap_err();
        assert!(error.contains("지원하지 않는 추론 레벨"));
    }

    #[test]
    fn bulk_요청은_모든_항목을_검증하고_중복을_거절한다() {
        assert_eq!(bulk(vec![], None).unwrap_err(), "직원 설정은 1개 이상 100개 이하여야 합니다.");
        assert!(normalize_bulk_character_settings(&json!({}), None).unwrap_err().to_string().contains("updates 배열"));
        assert!(normalize_bulk_character_settings(&json!([]), None).unwrap_err().to_string().contains("updates 배열"));
        let error = bulk(vec![settings("boss", json!({})), settings("boss", json!({}))], None).unwrap_err();
        assert_eq!(error, "같은 직원 설정을 두 번 보낼 수 없습니다: boss");
        let error = bulk(vec![settings("boss", json!({})), settings("right-man", json!({ "backend": "unknown" }))], None).unwrap_err();
        assert_eq!(error, "지원하지 않는 CLI입니다.");
        assert_eq!(
            normalize_character_settings_update(&json!("x"), true, None).unwrap_err().to_string(),
            "직원 설정은 JSON 객체여야 합니다."
        );
        assert_eq!(
            normalize_character_settings_update(&json!({ "characterId": "  " }), true, None).unwrap_err().to_string(),
            "직원 식별자를 입력하세요."
        );
    }

    #[test]
    fn antigravity_모델별_추론_권한_fast_계약을_검증한다() {
        let normalized = bulk(
            vec![settings("right-man", json!({ "backend": "antigravity", "model": "gemini-3.8-flash", "effort": "medium", "permission": "accept-edits" }))],
            None,
        )
        .unwrap();
        assert_eq!(
            normalized,
            vec![CharacterSettingsUpdate {
                character_id: "right-man".into(),
                backend: "antigravity".into(),
                model: "gemini-3.8-flash".into(),
                effort: "medium".into(),
                fast_mode: false,
                permission: "accept-edits".into(),
            }]
        );
        // 목록에서 내린 3.7-flash도 다시 저장할 수 있다.
        let kept = bulk(
            vec![settings("right-man", json!({ "backend": "antigravity", "model": "gemini-3.7-flash", "effort": "high", "permission": "accept-edits" }))],
            None,
        )
        .unwrap();
        assert_eq!(kept[0].model, "gemini-3.7-flash");
        for (patch, expected) in [
            (json!({ "effort": "medium" }), "지원하지 않는 추론 레벨입니다."),
            (json!({ "fastMode": true }), "선택한 CLI와 모델은 Fast 모드를 지원하지 않습니다."),
            (json!({ "permission": "auto" }), "지원하지 않는 권한입니다."),
            (json!({ "fastMode": "yes" }), "Fast 모드 설정은 참 또는 거짓이어야 합니다."),
            (json!({ "model": "gemini-9" }), "지원하지 않는 모델입니다."),
        ] {
            let mut base = settings("boss", json!({ "backend": "antigravity", "model": "gemini-3.1-pro", "effort": "high", "permission": "plan" }));
            base.as_object_mut().unwrap().extend(patch.as_object().unwrap().clone());
            assert_eq!(bulk(vec![base], None).unwrap_err(), expected);
        }
    }

    #[test]
    fn 동적_카탈로그의_새_모델과_옵션을_검증한다() {
        let catalog = FixedCatalog(vec![
            ("codex", "gpt-future-mini", ModelCapabilities { efforts: vec!["low".into(), "medium".into()], supports_fast_mode: false }),
            ("antigravity", "gemini-future-pro", ModelCapabilities { efforts: vec!["low".into(), "high".into()], supports_fast_mode: false }),
        ]);
        let normalized = bulk(
            vec![
                settings("boss", json!({ "model": "gpt-future-mini", "effort": "medium" })),
                settings("right-man", json!({ "backend": "antigravity", "model": "gemini-future-pro", "effort": "low", "permission": "accept-edits" })),
            ],
            Some(&catalog),
        )
        .unwrap();
        assert_eq!(normalized.iter().map(|u| (u.backend.as_str(), u.model.as_str(), u.effort.as_str())).collect::<Vec<_>>(), [
            ("codex", "gpt-future-mini", "medium"),
            ("antigravity", "gemini-future-pro", "low"),
        ]);
        let error = bulk(vec![settings("boss", json!({ "model": "gpt-future-mini", "effort": "ultra" }))], Some(&catalog)).unwrap_err();
        assert!(error.contains("지원하지 않는 추론 레벨"));
        let error = bulk(vec![settings("boss", json!({ "model": "gpt-future-mini", "effort": "medium", "fastMode": true }))], Some(&catalog)).unwrap_err();
        assert!(error.contains("Fast 모드"));
        // 카탈로그 efforts가 비면 내장 목록으로 돌아간다.
        let empty = FixedCatalog(vec![("codex", "gpt-5.6-sol", ModelCapabilities { efforts: vec![], supports_fast_mode: true })]);
        assert!(bulk(vec![settings("boss", json!({ "effort": "ultra", "fastMode": true }))], Some(&empty)).is_ok());
    }

    #[test]
    fn claude_accept_edits는_auto로_정규화한다() {
        let normalized = bulk(
            vec![settings("boss", json!({ "backend": "claude", "model": "claude-opus-5", "effort": "max", "fastMode": true, "permission": "acceptEdits" }))],
            None,
        )
        .unwrap();
        assert_eq!(normalized[0].permission, "auto");
        let serialized = serde_json::to_value(&normalized[0]).unwrap();
        assert_eq!(serialized["characterID"], "boss");
        assert_eq!(serialized["fastMode"], true);
    }

    #[test]
    fn 잠금_순서는_정렬과_중복_제거다() {
        let ids = ["right-man".to_string(), "boss".to_string(), "right-man".to_string()];
        assert_eq!(sorted_unique_character_ids(&ids), ["boss", "right-man"]);
    }

    #[test]
    fn 오류_문구는_node와_같다() {
        assert_eq!(
            CharacterSettingsError::TargetsNotFound(vec!["a".into(), "b".into()]).to_string(),
            "캐릭터를 찾을 수 없습니다: a, b"
        );
        assert_eq!(CharacterSettingsError::CharacterNotFound.to_string(), "캐릭터를 찾을 수 없습니다.");
        assert!(local_profile_busy(&json!({ "localProfileId": "local-4090" })).is_err());
        assert!(local_profile_busy(&json!({ "localProfileId": null })).is_ok());
        assert!(local_profile_busy(&json!({ "localProfileId": "" })).is_ok());
        assert!(local_profile_busy(&json!({})).is_ok());
    }
}
