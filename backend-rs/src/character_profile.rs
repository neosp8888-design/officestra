// 직원 이름·역할 지침·자동 압축 기준 변경. Node server.mjs의 updateCharacterName·updateCharacterIdentityPrompt·
// updateCharacterContextSettings와 같은 검증·쿼리·응답이다.

use serde_json::{Value, json};
use sqlx::{PgPool, Row};

use crate::db::with_transaction;
use crate::js::{is_js_integer, js_length, js_property, js_string_or_empty, js_to_number, js_trim};
use crate::rag::database_message;

const CHARACTER_NOT_FOUND: &str = "캐릭터를 찾을 수 없습니다.";

#[derive(Debug, PartialEq)]
pub enum ProfileError {
    /// 400.
    Validation(&'static str),
    /// 404 "캐릭터를 찾을 수 없습니다.".
    NotFound,
    /// 500. null 본문의 TypeError나 DB 오류 문구다.
    Internal(String),
}

impl ProfileError {
    pub fn status(&self) -> u16 {
        match self {
            Self::Validation(_) => 400,
            Self::NotFound => 404,
            Self::Internal(_) => 500,
        }
    }

    pub fn message(&self) -> String {
        match self {
            Self::Validation(message) => (*message).to_string(),
            Self::NotFound => CHARACTER_NOT_FOUND.into(),
            Self::Internal(message) => message.clone(),
        }
    }
}

impl From<sqlx::Error> for ProfileError {
    fn from(error: sqlx::Error) -> Self {
        Self::Internal(database_message(&error))
    }
}

/// `String(body.key ?? "").trim()`.
fn trimmed_text(body: &Value, key: &str) -> Result<String, ProfileError> {
    let value = js_property(body, key).map_err(ProfileError::Internal)?;
    Ok(js_trim(&js_string_or_empty(value)).to_string())
}

/// PUT /api/characters/:id/name.
pub async fn update_character_name(pool: &PgPool, character_id: &str, body: &Value) -> Result<Value, ProfileError> {
    let name = trimmed_text(body, "name")?;
    let length = js_length(&name);
    if length == 0 || length > 30 {
        return Err(ProfileError::Validation("이름은 1자 이상 30자 이하여야 합니다."));
    }
    let row = sqlx::query("UPDATE characters SET name = $2, updated_at = now() WHERE id = $1 RETURNING id, name")
        .bind(character_id)
        .bind(&name)
        .fetch_optional(pool)
        .await?
        .ok_or(ProfileError::NotFound)?;
    Ok(json!({ "id": row.try_get::<String, _>("id")?, "name": row.try_get::<String, _>("name")? }))
}

/// PUT /api/characters/:id/identity-prompt. 같은 값이면 갱신 시각을 바꾸지 않고 지금 값을 돌려준다.
pub async fn update_character_identity_prompt(pool: &PgPool, character_id: &str, body: &Value) -> Result<Value, ProfileError> {
    let identity_prompt = trimmed_text(body, "identityPrompt")?;
    let length = js_length(&identity_prompt);
    if length == 0 || length > 1200 {
        return Err(ProfileError::Validation("역할·업무 지침은 1자 이상 1,200자 이하여야 합니다."));
    }
    let character = with_transaction(pool, async |connection| {
        let Some(current) = sqlx::query("SELECT id, identity_prompt FROM characters WHERE id = $1 FOR UPDATE")
            .bind(character_id)
            .fetch_optional(&mut *connection)
            .await?
        else {
            return Ok::<_, ProfileError>(None);
        };
        let row = if current.try_get::<String, _>("identity_prompt")? == identity_prompt {
            current
        } else {
            sqlx::query(
                "UPDATE characters SET identity_prompt = $2, updated_at = now() WHERE id = $1 RETURNING id, identity_prompt",
            )
            .bind(character_id)
            .bind(&identity_prompt)
            .fetch_one(&mut *connection)
            .await?
        };
        Ok(Some(json!({
            "id": row.try_get::<String, _>("id")?,
            "identityPrompt": row.try_get::<String, _>("identity_prompt")?,
        })))
    })
    .await?;
    character.ok_or(ProfileError::NotFound)
}

/// PUT /api/characters/:id/context-settings. 클라우드 모델은 95%까지, 로컬 모델 직원만 100%까지 받는다.
pub async fn update_character_context_settings(pool: &PgPool, character_id: &str, body: &Value) -> Result<Value, ProfileError> {
    let value = js_property(body, "autoCompactPercent").map_err(ProfileError::Internal)?;
    let percent = value.map_or(f64::NAN, js_to_number);
    if !is_js_integer(percent) || !(20.0..=100.0).contains(&percent) {
        return Err(ProfileError::Validation("자동 압축 기준은 20% 이상 100% 이하의 정수여야 합니다."));
    }
    let row = sqlx::query(
        "UPDATE characters SET auto_compact_percent = $2, updated_at = now()
         WHERE id = $1 AND ($2 <= 95 OR NULLIF(config->>'localProfileId', '') IS NOT NULL)
         RETURNING id, auto_compact_percent",
    )
    .bind(character_id)
    .bind(percent as i16)
    .fetch_optional(pool)
    .await?;
    let Some(row) = row else {
        let exists = sqlx::query("SELECT id FROM characters WHERE id = $1").bind(character_id).fetch_optional(pool).await?;
        return Err(if exists.is_some() {
            ProfileError::Validation("클라우드 모델의 자동 압축 기준은 최대 95%입니다.")
        } else {
            ProfileError::NotFound
        });
    };
    Ok(json!({
        "id": row.try_get::<String, _>("id")?,
        "autoCompactPercent": row.try_get::<i16, _>("auto_compact_percent")?,
    }))
}
