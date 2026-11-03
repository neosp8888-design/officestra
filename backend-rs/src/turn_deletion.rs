// 보관함에서 선택한 종료 턴과 그 턴에서 파생된 기록을 한 트랜잭션으로 삭제한다. Node turn-deletion.mjs를 옮겼다.

use std::fmt;

use serde_json::{Value, json};
use sqlx::{PgPool, Row};

use crate::db::with_transaction;
use crate::js::js_trim;
use crate::rag::database_message;

#[derive(Debug)]
pub enum TurnDeletionError {
    /// TurnDeletionError. 상태 코드와 문구를 그대로 응답한다.
    Rejected { status: u16, message: String },
    /// Node의 일반 Error(500).
    Failed(String),
    Database(sqlx::Error),
}

impl TurnDeletionError {
    pub fn status(&self) -> u16 {
        match self {
            Self::Rejected { status, .. } => *status,
            Self::Failed(_) | Self::Database(_) => 500,
        }
    }
}

impl fmt::Display for TurnDeletionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Rejected { message, .. } | Self::Failed(message) => formatter.write_str(message),
            Self::Database(error) => formatter.write_str(&database_message(error)),
        }
    }
}

impl From<sqlx::Error> for TurnDeletionError {
    fn from(error: sqlx::Error) -> Self {
        Self::Database(error)
    }
}

fn rejected(status: u16, message: &str) -> TurnDeletionError {
    TurnDeletionError::Rejected { status, message: message.into() }
}

#[derive(Debug, PartialEq)]
pub struct DeletedTurn {
    pub id: String,
    pub character_id: String,
    pub deleted_work_record_count: u64,
    pub session_reset: bool,
}

impl DeletedTurn {
    /// DELETE /api/turns/:id 응답. `{deleted: true, ...deleted}` 순서다.
    pub fn response(&self) -> Value {
        json!({
            "deleted": true,
            "id": self.id,
            "characterId": self.character_id,
            "deletedWorkRecordCount": self.deleted_work_record_count,
            "sessionReset": self.session_reset,
        })
    }
}

/// deleteArchivedTurn. 진행 중인 턴이나 같은 세션에 진행 중인 턴이 있으면 거절하고, 활성 세션이었으면 세션 재개를 끊는다.
/// 턴이 없으면 None이다.
pub async fn delete_archived_turn(pool: &PgPool, turn_id: &str) -> Result<Option<DeletedTurn>, TurnDeletionError> {
    let normalized = js_trim(turn_id);
    if normalized.is_empty() {
        return Err(rejected(400, "삭제할 대화를 찾을 수 없습니다."));
    }
    with_transaction(pool, async |connection| {
        let Some(selected) = sqlx::query(
            "SELECT turn.id::text AS id, turn.status, turn.cli_session_id::text AS session_id,
               session.character_id
             FROM turns AS turn
             JOIN cli_sessions AS session ON session.id = turn.cli_session_id
             WHERE turn.id::text = $1
             FOR UPDATE OF turn, session",
        )
        .bind(normalized)
        .fetch_optional(&mut *connection)
        .await?
        else {
            return Ok(None);
        };
        let id: String = selected.try_get("id")?;
        let status: String = selected.try_get("status")?;
        let session_id: String = selected.try_get("session_id")?;
        let character_id: String = selected.try_get("character_id")?;
        if status == "pending" || status == "running" {
            return Err(rejected(409, "진행 중인 대화는 삭제할 수 없습니다."));
        }
        let running = sqlx::query(
            "SELECT 1 FROM turns WHERE cli_session_id = $1::uuid AND status IN ('pending', 'running') LIMIT 1",
        )
        .bind(&session_id)
        .fetch_optional(&mut *connection)
        .await?;
        if running.is_some() {
            return Err(rejected(409, "같은 세션에서 대화가 진행 중이라 삭제할 수 없습니다."));
        }
        let record_ids: Vec<String> =
            sqlx::query_scalar("SELECT id::text FROM work_records WHERE source_turn_id = $1::uuid FOR UPDATE")
                .bind(&id)
                .fetch_all(&mut *connection)
                .await?;
        let session_reset = sqlx::query("DELETE FROM active_cli_sessions WHERE cli_session_id = $1::uuid")
            .bind(&session_id)
            .execute(&mut *connection)
            .await?
            .rows_affected()
            > 0;
        if session_reset {
            sqlx::query("UPDATE cli_sessions SET ended_at = COALESCE(ended_at, now()) WHERE id = $1::uuid")
                .bind(&session_id)
                .execute(&mut *connection)
                .await?;
        }
        sqlx::query("DELETE FROM turn_response_sources WHERE work_record_id = ANY($1::uuid[])")
            .bind(&record_ids)
            .execute(&mut *connection)
            .await?;
        sqlx::query("DELETE FROM wiki_proposals WHERE author_turn_id = $1::uuid OR target_record_id = ANY($2::uuid[])")
            .bind(&id)
            .bind(&record_ids)
            .execute(&mut *connection)
            .await?;
        sqlx::query("DELETE FROM work_record_events WHERE source_turn_id = $1::uuid OR record_id = ANY($2::uuid[])")
            .bind(&id)
            .bind(&record_ids)
            .execute(&mut *connection)
            .await?;
        sqlx::query(
            "DELETE FROM work_record_links WHERE source_record_id = ANY($1::uuid[]) OR target_record_id = ANY($1::uuid[])",
        )
        .bind(&record_ids)
        .execute(&mut *connection)
        .await?;
        let deleted_work_record_count = sqlx::query("DELETE FROM work_records WHERE id = ANY($1::uuid[])")
            .bind(&record_ids)
            .execute(&mut *connection)
            .await?
            .rows_affected();
        let deleted_turns =
            sqlx::query("DELETE FROM turns WHERE id = $1::uuid").bind(&id).execute(&mut *connection).await?.rows_affected();
        if deleted_turns != 1 {
            return Err(TurnDeletionError::Failed("대화 삭제 결과를 확인하지 못했습니다.".into()));
        }
        Ok(Some(DeletedTurn { id, character_id, deleted_work_record_count, session_reset }))
    })
    .await
}
