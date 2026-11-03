// 완료된 대화의 사용자 평가를 검증하고 PostgreSQL에 저장한다. Node turn-feedback.mjs를 옮겼다.

use serde_json::Value;
use sqlx::PgConnection;

const INVALID_FEEDBACK: &str = "feedback 값은 liked, disliked 또는 null이어야 합니다.";

/// replaceTurnFeedback의 결과. 평가가 없는 보통 상태는 행 부재(None)로 나타낸다.
#[derive(Debug, PartialEq)]
pub enum FeedbackOutcome {
    Missing,
    Unavailable,
    Stored(Option<String>),
}

/// normalizeTurnFeedback. null은 평가 지우기이고, 그 밖에는 liked·disliked 문자열만 받는다(없는 값도 거절).
pub fn normalize_turn_feedback(value: Option<&Value>) -> Result<Option<String>, String> {
    match value {
        Some(Value::Null) => Ok(None),
        Some(Value::String(feedback)) if feedback == "liked" || feedback == "disliked" => Ok(Some(feedback.clone())),
        _ => Err(INVALID_FEEDBACK.into()),
    }
}

/// replaceTurnFeedback. 턴을 잠그고 완료된 턴에만 평가를 한 행으로 덮어쓰거나 지운다.
pub async fn replace_turn_feedback(
    connection: &mut PgConnection,
    turn_id: &str,
    feedback: Option<&str>,
) -> Result<FeedbackOutcome, sqlx::Error> {
    let status: Option<String> = sqlx::query_scalar("SELECT status FROM turns WHERE id = $1::uuid FOR UPDATE")
        .bind(turn_id)
        .fetch_optional(&mut *connection)
        .await?;
    let Some(status) = status else {
        return Ok(FeedbackOutcome::Missing);
    };
    if status != "completed" {
        return Ok(FeedbackOutcome::Unavailable);
    }
    let Some(feedback) = feedback else {
        sqlx::query("DELETE FROM turn_response_feedback WHERE turn_id = $1::uuid")
            .bind(turn_id)
            .execute(&mut *connection)
            .await?;
        return Ok(FeedbackOutcome::Stored(None));
    };
    let stored: String = sqlx::query_scalar(
        "INSERT INTO turn_response_feedback (turn_id, feedback) VALUES ($1::uuid, $2)
         ON CONFLICT (turn_id) DO UPDATE SET feedback = EXCLUDED.feedback, updated_at = now()
         RETURNING feedback",
    )
    .bind(turn_id)
    .bind(feedback)
    .fetch_one(&mut *connection)
    .await?;
    Ok(FeedbackOutcome::Stored(Some(stored)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn 대화_평가는_좋아요_싫어요_미선택만_허용한다() {
        assert_eq!(normalize_turn_feedback(Some(&json!("liked"))), Ok(Some("liked".into())));
        assert_eq!(normalize_turn_feedback(Some(&json!("disliked"))), Ok(Some("disliked".into())));
        assert_eq!(normalize_turn_feedback(Some(&Value::Null)), Ok(None));
        for invalid in [None, Some(json!("neutral")), Some(json!("")), Some(json!(1)), Some(json!(["liked"])), Some(json!(false))] {
            assert_eq!(normalize_turn_feedback(invalid.as_ref()).unwrap_err(), INVALID_FEEDBACK);
        }
    }
}
