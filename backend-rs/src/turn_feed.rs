// 활성 세션·대화 기록·업무 기록·응답 출처·실시간/보관함 피드 조회와 응답 출처 교체. Node server.mjs의 해당 함수를 옮겼다.

use std::collections::HashMap;
use std::sync::LazyLock;

use serde_json::{Map, Value, json};
use sqlx::{AssertSqlSafe, PgPool};

use crate::db::with_transaction;
use crate::js::{SearchParams, date_parse, js_max, js_min, js_property, js_trim, number_to_string, string_to_number};
use crate::local_artifacts::{append_local_image_previews, generated_image_root, generated_images_for_turn};
use crate::local_model_capabilities::{MEROMERO_26B_MODEL_ID, MEROMERO_MODEL_ID};
use crate::session_context_usage::{ContextRequest, TranscriptRoots, session_context_usage};
use crate::usage_report::TurnCostSummaryCache;
use crate::work_record_provenance::{
    ProvenanceValidationError, SourceStoreError, is_uuid, normalize_response_sources, parse_work_record_filters,
    replace_turn_response_sources,
};

const LIVE_FEED_MINIMUM_TURNS_PER_CHARACTER: i32 = 10;
const LIFECYCLE_STATE_MEANING: &str =
    "기록의 보존 상태입니다. active는 실행 중이라는 뜻이 아닙니다. 직원 실행 상태는 GET /api/agent-jobs로 조회하세요.";
const NOT_FOUND_MESSAGE: &str = "대화를 찾을 수 없습니다.";

#[derive(Debug)]
pub enum FeedError {
    Validation(String),
    NotFound,
    /// 직원별 기록의 대상 직원이 없다. "캐릭터를 찾을 수 없습니다."(404).
    CharacterNotFound,
    /// 지금 상태로는 바꿀 수 없다(409).
    Conflict(String),
    Database(sqlx::Error),
    /// 파일 읽기 등 그 밖의 실패. Node는 500과 오류 문구를 낸다.
    Internal(String),
}

impl FeedError {
    pub fn status(&self) -> u16 {
        match self {
            Self::Validation(_) => 400,
            Self::NotFound | Self::CharacterNotFound => 404,
            Self::Conflict(_) => 409,
            Self::Database(_) | Self::Internal(_) => 500,
        }
    }

    pub fn message(&self) -> String {
        match self {
            Self::Validation(message) | Self::Conflict(message) | Self::Internal(message) => message.clone(),
            Self::NotFound => NOT_FOUND_MESSAGE.into(),
            Self::CharacterNotFound => "캐릭터를 찾을 수 없습니다.".into(),
            // node-pg의 error.message는 서버 문구만 담는다.
            Self::Database(error) => match error.as_database_error() {
                Some(database_error) => database_error.message().to_string(),
                None => error.to_string(),
            },
        }
    }
}

impl From<sqlx::Error> for FeedError {
    fn from(error: sqlx::Error) -> Self {
        Self::Database(error)
    }
}

impl From<ProvenanceValidationError> for FeedError {
    fn from(error: ProvenanceValidationError) -> Self {
        Self::Validation(error.0)
    }
}

/// 최상위 timestamptz는 node-pg가 Date로 바꿔 JSON에서 toISOString 꼴이 된다.
pub(crate) fn iso(column: &str) -> String {
    format!(r#"to_char({column} AT TIME ZONE 'UTC', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"')"#)
}

/// Node local-provider-service.mjs의 LOCAL_TURN_EFFORT_SQL. 저장된 실행 설정을 표시용으로만 해석한다.
pub static LOCAL_TURN_EFFORT_SQL: LazyLock<String> = LazyLock::new(|| {
    format!(
        "CASE
  WHEN t.provider_kind = 'local' AND t.effort = 'default'
    AND t.provider_snapshot->'profile'->>'runtime' = 'llama-cpp-b10982'
  THEN CASE
    WHEN t.provider_snapshot->'profile'->>'model' IN ('{MEROMERO_MODEL_ID}','{MEROMERO_26B_MODEL_ID}')
      THEN CASE WHEN t.provider_snapshot->'profile'->>'reasoning' = 'on' THEN 'on' ELSE 'off' END
    WHEN t.provider_snapshot->'profile'->>'reasoning' IN ('low','medium','xhigh')
      THEN t.provider_snapshot->'profile'->>'reasoning'
    WHEN COALESCE(t.provider_snapshot->'profile'->>'reasoning','default') = 'default' THEN 'xhigh'
    ELSE t.effort END
  ELSE t.effort END"
    )
});

const SOURCES_SQL: &str = "COALESCE(
          (
            SELECT json_agg(
              json_build_object(
                'id', source.id,
                'sourceKind', source.source_kind,
                'title', source.title,
                'locator', source.locator,
                'excerpt', source.excerpt,
                'ragDocumentId', source.rag_document_id,
                'workRecordId', source.work_record_id
              )
              ORDER BY source.ordinal, source.created_at, source.id
            )
            FROM turn_response_sources AS source
            WHERE source.turn_id = {turn}.id
          ),
          '[]'::json
        )";

fn sources_sql(turn: &str) -> String {
    SOURCES_SQL.replace("{turn}", turn)
}

const LATEST_RESPONSE_SQL: &str = "(
            SELECT text
            FROM messages
            WHERE turn_id = t.id
              AND role = 'assistant'
            ORDER BY received_at DESC
            LIMIT 1
          )";

/// 보관함 검색 대상 문장. Node와 같은 열을 같은 순서로 잇는다.
fn search_text_sql() -> String {
    format!(
        "concat_ws(
              ' ',
              character.name,
              t.prompt,
              t.backend,
              t.model,
              t.resolved_model,
              {effort},
              session.external_id,
              {response}
            )",
        effort = *LOCAL_TURN_EFFORT_SQL,
        response = LATEST_RESPONSE_SQL,
    )
}

// 매개변수는 모두 문자열로 보내고, node-pg(형식 없는 매개변수)에서 PG가 추론하던 형식으로 명시해 바꾼다.
// LIMIT/OFFSET은 bigint, `$4::integer`는 bigint를 다시 integer로 바꾼 것이다.

static ACTIVE_SESSIONS_SQL: LazyLock<String> = LazyLock::new(|| {
    format!(
        r#"SELECT row_to_json(x) FROM (
      SELECT
        active.character_id AS "characterId",
        session.external_id AS "externalSessionId",
        session.conversation_id AS "conversationId",
        {started_at} AS "startedAt"
      FROM active_cli_sessions AS active
      JOIN cli_sessions AS session
        ON session.id = active.cli_session_id
      WHERE session.external_id IS NOT NULL
        AND session.ended_at IS NULL
      ORDER BY active.character_id
    ) AS x"#,
        started_at = iso("session.started_at"),
    )
});

static HISTORY_SQL: LazyLock<String> = LazyLock::new(|| {
    format!(
        r#"SELECT row_to_json(x) FROM (
      SELECT
        t.id,
        c.id AS "characterId",
        c.name AS "characterName",
        c.backend,
        t.backend AS "executionBackend",
        t.model AS "executionModel",
        t.resolved_model AS "resolvedModel",
        {effort} AS "executionEffort",
        t.fast_mode AS "executionFastMode",
        t.origin,
        (SELECT jsonb_build_object('characterId', sender.id, 'name', sender.name)
         FROM characters sender WHERE sender.id = t.sender_character_id) AS "promptSender",
        t.request_source AS "requestSource",
        t.provider_kind AS "providerKind",
        (t.provider_snapshot->'profile'->>'contextWindow')::integer AS "providerContextWindow",
        s.external_id AS "externalSessionId",
        COALESCE(
          CASE
            WHEN history_workspace.status IN ('merged', 'closed')
              THEN history_workspace.source_workdir
            ELSE history_workspace.execution_workdir
          END,
          conversation.workdir
        ) AS "conversationWorkdir",
        t.prompt,
        COALESCE({response}, '') AS response,
        {sources} AS sources,
        {started_at} AS "startedAt",
        {ended_at} AS "endedAt",
        t.response_source_warning AS "responseSourceWarning",
        t.wiki_proposal_warning AS "wikiProposalWarning"
      FROM turns t
      JOIN cli_sessions s ON s.id = t.cli_session_id
      JOIN conversations AS conversation
        ON conversation.id = s.conversation_id
      LEFT JOIN task_workspaces AS history_workspace
        ON history_workspace.id = t.task_workspace_id
      JOIN characters c ON c.id = s.character_id
      WHERE ($1::text IS NULL OR c.id = $1::text)
        AND ($2::timestamptz IS NULL OR t.started_at >= $2::timestamptz)
        AND ($3::timestamptz IS NULL OR t.started_at <= $3::timestamptz)
      ORDER BY t.started_at DESC
      LIMIT 1000
    ) AS x"#,
        effort = *LOCAL_TURN_EFFORT_SQL,
        response = LATEST_RESPONSE_SQL,
        sources = sources_sql("t"),
        started_at = iso("t.started_at"),
        ended_at = iso("t.ended_at"),
    )
});

// 묶음 안의 기록 시각은 row_to_json 문자열 그대로라 Node와 같다.
const WORK_RECORDS_SQL: &str = r#"
      WITH matching AS (
        SELECT
          record.id,
          record.project_id AS "projectId",
          project.name AS "projectName",
          project.repository_root AS "repositoryRoot",
          record.record_type AS "recordType",
          record.lifecycle_state AS "lifecycleState",
          record.title,
          record.body,
          record.legacy_stage_number AS "legacyStageNumber",
          record.character_id AS "characterId",
          record.attribution,
          record.source_turn_id AS "sourceTurnId",
          record.source_workspace_id AS "sourceWorkspaceId",
          record.source_path AS "sourcePath",
          record.source_commit AS "sourceCommit",
          record.source_section_ordinal AS "sourceSectionOrdinal",
          record.source_line_start AS "sourceLineStart",
          record.source_line_end AS "sourceLineEnd",
          record.source_section_sha256 AS "sourceSectionSha256",
          record.metadata,
          record.recorded_at AS "recordedAt",
          record.updated_at AS "updatedAt",
          CASE
            WHEN $5::text IS NULL THEN NULL
            ELSE ts_rank(
              record.search_document,
              websearch_to_tsquery('simple', $5::text)
            )::double precision
          END AS score,
          COALESCE(
            (
              SELECT json_agg(
                json_build_object(
                  'ordinal', item.ordinal,
                  'text', item.item_text,
                  'isChecked', item.is_checked,
                  'metadata', item.metadata
                )
                ORDER BY item.ordinal
              )
              FROM work_record_items AS item
              WHERE item.record_id = record.id
            ),
            '[]'::json
          ) AS items
        FROM work_records AS record
        JOIN projects AS project
          ON project.id = record.project_id
        WHERE ($1::uuid IS NULL OR record.project_id = $1::uuid)
          AND ($2::text IS NULL OR record.record_type = $2::text)
          AND ($3::text IS NULL OR record.lifecycle_state = $3::text)
          AND ($4::text IS NULL OR record.attribution = $4::text)
          AND (
            $5::text IS NULL
            OR record.search_document @@ websearch_to_tsquery('simple', $5::text)
          )
      ), page AS (
        SELECT *
        FROM matching
        ORDER BY score DESC NULLS LAST, "recordedAt" DESC, id
        LIMIT $6::bigint
        OFFSET $7::bigint
      )
      SELECT
        (SELECT count(*)::integer FROM matching) AS total,
        COALESCE(
          (
            SELECT json_agg(
              row_to_json(page)
              ORDER BY score DESC NULLS LAST, "recordedAt" DESC, id
            )
            FROM page
          ),
          '[]'::json
        ) AS records
    "#;

static TURN_SOURCES_SQL: LazyLock<String> = LazyLock::new(|| {
    format!(
        "SELECT turn.response_source_warning AS warning, {sources} AS sources
      FROM turns AS turn
      WHERE turn.id = $1::uuid",
        sources = sources_sql("turn"),
    )
});

static TURN_FEED_SQL: LazyLock<String> = LazyLock::new(|| {
    format!(
        r#"
      WITH matching_turn_ids AS (
        SELECT t.id, t.started_at, session.character_id
        FROM turns AS t
        JOIN cli_sessions AS session
          ON session.id = t.cli_session_id
        JOIN characters AS character
          ON character.id = session.character_id
        WHERE $1::uuid IS NULL
          AND (
            $3::text IS NULL
            OR {search_text} ILIKE '%' || $3::text || '%'
          )
      ), ranked_character_turn_ids AS (
        SELECT
          matching.id,
          row_number() OVER (
            PARTITION BY matching.character_id
            ORDER BY matching.started_at DESC, matching.id DESC
          ) AS character_rank
        FROM matching_turn_ids AS matching
      ), selected_turn_ids AS (
        SELECT recent.id
        FROM (
          SELECT id
          FROM matching_turn_ids
          ORDER BY started_at DESC, id DESC
          LIMIT $2::bigint
          OFFSET $4::bigint
        ) AS recent
        UNION
        SELECT ranked.id
        FROM ranked_character_turn_ids AS ranked
        WHERE $1::uuid IS NULL
          AND $5::boolean
          AND $3::text IS NULL
          AND $4::bigint::integer = 0
          AND ranked.character_rank <= $6::integer
        UNION
        SELECT $1::uuid
        WHERE $1::uuid IS NOT NULL
      )
      SELECT row_to_json(x) FROM (
      SELECT
        t.id,
        c.id AS "characterId",
        c.name AS "characterName",
        c.backend AS "characterBackend",
        t.backend,
        t.model,
        t.resolved_model AS "resolvedModel",
        {effort} AS effort,
        t.fast_mode AS "fastMode",
        t.origin,
        (SELECT jsonb_build_object('characterId', sender.id, 'name', sender.name)
         FROM characters sender WHERE sender.id = t.sender_character_id) AS "promptSender",
        t.request_source AS "requestSource",
        t.provider_kind AS "providerKind",
        (t.provider_snapshot->'profile'->>'contextWindow')::integer AS "providerContextWindow",
        s.external_id AS "externalSessionId",
        conversation.workdir AS "conversationWorkdir",
        t.prompt,
        t.status,
        t.needs_input AS "needsInput",
        t.error_message AS "errorMessage",
        t.response_source_warning AS "responseSourceWarning",
        t.wiki_proposal_warning AS "wikiProposalWarning",
        turn_feedback.feedback,
        {started_at} AS "startedAt",
        {ended_at} AS "endedAt",
        {updated_at} AS "updatedAt",
        usage.cost_usd::double precision AS "estimatedCostUsd",
        workspace.review AS workspace,
        COALESCE({response}, '') AS response,
        COALESCE(
          (
            SELECT json_agg(
              json_build_object(
                'id', activity.id,
                'kind', activity.kind,
                'text', activity.text,
                'status', activity.status,
                'collaboration', activity.collaboration,
                'occurredAt', activity.occurred_at
              )
              ORDER BY activity.seq
            )
            FROM turn_activities AS activity
            WHERE activity.turn_id = t.id
          ),
          '[]'::json
        ) AS activities,
        {sources} AS sources
      FROM selected_turn_ids AS selected_turn
      JOIN turns AS t
        ON t.id = selected_turn.id
      JOIN cli_sessions AS s
        ON s.id = t.cli_session_id
      JOIN conversations AS conversation
        ON conversation.id = s.conversation_id
      JOIN characters AS c
        ON c.id = s.character_id
      LEFT JOIN usage_records AS usage
        ON usage.turn_id = t.id
      LEFT JOIN turn_response_feedback AS turn_feedback
        ON turn_feedback.turn_id = t.id
      LEFT JOIN LATERAL (
        SELECT json_build_object(
          'status', task_workspace.status,
          'repositoryRoot', task_workspace.repository_root,
          'worktreePath', task_workspace.worktree_path,
          'executionWorkdir', CASE
            WHEN task_workspace.status IN ('merged', 'closed')
              THEN task_workspace.source_workdir
            ELSE task_workspace.execution_workdir
          END,
          'branchName', task_workspace.branch_name,
          'baseBranch', task_workspace.base_branch,
          'baseCommit', task_workspace.base_commit,
          'reviewTree', task_workspace.review_tree,
          'headCommit', task_workspace.head_commit,
          'changedFiles', task_workspace.changed_files,
          'mergedCommit', task_workspace.merged_commit,
          'errorMessage', task_workspace.error_message
        ) AS review
        FROM task_workspaces AS task_workspace
        WHERE task_workspace.review_turn_id = t.id
          OR (
            task_workspace.review_turn_id IS NULL
            AND task_workspace.id = t.task_workspace_id
          )
        LIMIT 1
      ) AS workspace ON true
      ORDER BY t.started_at DESC, t.id DESC
      ) AS x"#,
        search_text = search_text_sql(),
        effort = *LOCAL_TURN_EFFORT_SQL,
        started_at = iso("t.started_at"),
        ended_at = iso("t.ended_at"),
        updated_at = iso("t.updated_at"),
        response = LATEST_RESPONSE_SQL,
        sources = sources_sql("t"),
    )
});

static ARCHIVE_COUNT_SQL: LazyLock<String> = LazyLock::new(|| {
    format!(
        "SELECT count(*)::integer AS total
        FROM turns AS t
        JOIN cli_sessions AS session
          ON session.id = t.cli_session_id
        JOIN characters AS character
          ON character.id = session.character_id
        WHERE (
          $1::text IS NULL
          OR {search_text} ILIKE '%' || $1::text || '%'
        )",
        search_text = search_text_sql(),
    )
});

// 정렬이 없는 Node 질의 그대로다. 같은 턴의 전달 목록 순서는 PG가 돌려준 순서를 따른다.
const REPLY_DELIVERIES_SQL: &str = r#"SELECT delivery.source_turn_id::text,
    delivery.recipient_character_id::text, recipient.name::text,
    delivery.status::text, delivery.error_message::text
    FROM reply_deliveries delivery JOIN characters recipient ON recipient.id=delivery.recipient_character_id
    WHERE delivery.source_turn_id=ANY($1::uuid[])"#;

/// json!은 값을 참조로 받아 복사하므로 큰 목록은 이 함수로 옮겨 담는다. 키 순서는 넣은 순서다.
fn object<const N: usize>(pairs: [(&str, Value); N]) -> Value {
    Value::Object(pairs.into_iter().map(|(key, value)| (key.to_string(), value)).collect())
}

pub async fn list_active_sessions(pool: &PgPool) -> Result<Value, FeedError> {
    let sessions: Vec<Value> = sqlx::query_scalar(AssertSqlSafe(ACTIVE_SESSIONS_SQL.as_str())).fetch_all(pool).await?;
    Ok(object([("sessions", Value::Array(sessions))]))
}

/// `url.searchParams.get(name) || null`
fn non_empty(params: &SearchParams, name: &str) -> Option<String> {
    params.get(name).filter(|value| !value.is_empty()).map(str::to_string)
}

pub async fn global_history(pool: &PgPool, params: &SearchParams) -> Result<Value, FeedError> {
    let rows: Vec<Value> = sqlx::query_scalar(AssertSqlSafe(HISTORY_SQL.as_str()))
        .bind(non_empty(params, "characterId"))
        .bind(non_empty(params, "from"))
        .bind(non_empty(params, "to"))
        .fetch_all(pool)
        .await?;
    let turns: Vec<Value> = blocking(move || rows.into_iter().map(with_artifact_previews).collect()).await?;
    Ok(object([("turns", Value::Array(turns))]))
}

static CHARACTER_HISTORY_SESSIONS_SQL: LazyLock<String> = LazyLock::new(|| {
    format!(
        r#"SELECT row_to_json(x) FROM (
      SELECT id, external_id AS "externalId", {started_at} AS "startedAt", {ended_at} AS "endedAt"
      FROM cli_sessions
      WHERE character_id = $1
      ORDER BY started_at DESC
    ) AS x"#,
        started_at = iso("started_at"),
        ended_at = iso("ended_at"),
    )
});

static CHARACTER_HISTORY_TURNS_SQL: LazyLock<String> = LazyLock::new(|| {
    format!(
        r#"SELECT row_to_json(x) FROM (
      SELECT
        t.id,
        t.cli_session_id AS "sessionId",
        t.backend AS "executionBackend",
        t.model AS "executionModel",
        t.resolved_model AS "resolvedModel",
        {effort} AS "executionEffort",
        t.fast_mode AS "executionFastMode",
        t.origin,
        (SELECT jsonb_build_object('characterId', sender.id, 'name', sender.name)
         FROM characters sender WHERE sender.id = t.sender_character_id) AS "promptSender",
        t.request_source AS "requestSource",
        t.provider_kind AS "providerKind",
        (t.provider_snapshot->'profile'->>'contextWindow')::integer AS "providerContextWindow",
        COALESCE(
          CASE
            WHEN history_workspace.status IN ('merged', 'closed')
              THEN history_workspace.source_workdir
            ELSE history_workspace.execution_workdir
          END,
          conversation.workdir
        ) AS "conversationWorkdir",
        t.prompt,
        {started_at} AS "startedAt",
        {ended_at} AS "endedAt",
        t.response_source_warning AS "responseSourceWarning",
        t.wiki_proposal_warning AS "wikiProposalWarning",
        COALESCE({response}, '') AS response,
        {sources} AS sources
      FROM turns t
      JOIN cli_sessions s ON s.id = t.cli_session_id
      JOIN conversations AS conversation
        ON conversation.id = s.conversation_id
      LEFT JOIN task_workspaces AS history_workspace
        ON history_workspace.id = t.task_workspace_id
      WHERE s.character_id = $1
      ORDER BY t.started_at DESC
    ) AS x"#,
        effort = *LOCAL_TURN_EFFORT_SQL,
        response = LATEST_RESPONSE_SQL,
        sources = sources_sql("t"),
        started_at = iso("t.started_at"),
        ended_at = iso("t.ended_at"),
    )
});

/// GET /api/characters/:id/history. 직원의 CLI 세션을 최신순으로, 세션마다 그 턴을 최신순으로 묶는다.
/// 턴에는 backend가 없어 Node처럼 생성 이미지는 Codex 폴더에서 찾는다.
pub async fn character_history(pool: &PgPool, character_id: &str) -> Result<Value, FeedError> {
    let character: Option<Value> = sqlx::query_scalar(
        "SELECT row_to_json(x) FROM (SELECT id, name, seat, backend FROM characters WHERE id = $1) AS x",
    )
    .bind(character_id)
    .fetch_optional(pool)
    .await?;
    let Some(character) = character else {
        return Err(FeedError::CharacterNotFound);
    };
    let sessions: Vec<Value> = sqlx::query_scalar(AssertSqlSafe(CHARACTER_HISTORY_SESSIONS_SQL.as_str()))
        .bind(character_id)
        .fetch_all(pool)
        .await?;
    let turns: Vec<Value> = sqlx::query_scalar(AssertSqlSafe(CHARACTER_HISTORY_TURNS_SQL.as_str()))
        .bind(character_id)
        .fetch_all(pool)
        .await?;
    let sessions: Vec<Value> = blocking(move || {
        sessions
            .into_iter()
            .map(|mut session| {
                let session_id = session.get("id").and_then(Value::as_str).map(str::to_string);
                let external_id = session.get("externalId").and_then(Value::as_str).map(str::to_string);
                let session_turns = turns
                    .iter()
                    .filter(|turn| turn.get("sessionId").and_then(Value::as_str) == session_id.as_deref())
                    .map(|turn| with_session_artifact_previews(turn.clone(), external_id.as_deref()))
                    .collect::<Result<Vec<_>, _>>()?;
                if let Some(fields) = session.as_object_mut() {
                    fields.insert("turns".into(), Value::Array(session_turns));
                }
                Ok(session)
            })
            .collect()
    })
    .await?;
    Ok(object([("character", character), ("sessions", Value::Array(sessions))]))
}

pub async fn list_work_records(pool: &PgPool, params: &SearchParams) -> Result<Value, FeedError> {
    let filters = parse_work_record_filters(params)?;
    let (total, records): (i32, Value) = sqlx::query_as(AssertSqlSafe(WORK_RECORDS_SQL))
        .bind(&filters.project_id)
        .bind(&filters.record_type)
        .bind(&filters.lifecycle_state)
        .bind(&filters.attribution)
        .bind(&filters.query)
        .bind(number_to_string(filters.limit))
        .bind(number_to_string(filters.offset))
        .fetch_one(pool)
        .await?;
    Ok(object([
        ("records", records),
        ("lifecycleStateMeaning", LIFECYCLE_STATE_MEANING.into()),
        ("total", total.into()),
        ("limit", filters.limit.into()),
        ("offset", filters.offset.into()),
    ]))
}

pub async fn list_turn_sources(pool: &PgPool, turn_id: &str) -> Result<Value, FeedError> {
    if !is_uuid(turn_id) {
        return Err(FeedError::Validation("turnId 값은 UUID여야 합니다.".into()));
    }
    let row: Option<(Option<String>, Value)> =
        sqlx::query_as(AssertSqlSafe(TURN_SOURCES_SQL.as_str())).bind(turn_id).fetch_optional(pool).await?;
    let (warning, sources) = row.ok_or(FeedError::NotFound)?;
    Ok(object([("sources", sources), ("warning", warning.into())]))
}

/// PUT /api/turns/:id/sources(Node replaceTurnSources). 진행 중이 아닌 턴의 출처를 통째로 바꾸고 출처 경고를 지운다.
/// `body`는 요청 본문 그대로라, null이면 Node처럼 속성 읽기 오류(500)다.
pub async fn replace_turn_sources(pool: &PgPool, turn_id: &str, body: &Value) -> Result<Value, FeedError> {
    if !is_uuid(turn_id) {
        return Err(FeedError::Validation("turnId 값은 UUID여야 합니다.".into()));
    }
    let requested = js_property(body, "sources").map_err(FeedError::Internal)?;
    let sources = normalize_response_sources(requested.unwrap_or(&Value::Null), 50)?;
    let records = with_transaction(pool, async |connection| {
        let status: Option<String> = sqlx::query_scalar("SELECT status FROM turns WHERE id = $1::uuid FOR UPDATE")
            .bind(turn_id)
            .fetch_optional(&mut *connection)
            .await?;
        match status.as_deref() {
            None => return Err(FeedError::NotFound),
            Some("pending" | "running") => {
                return Err(FeedError::Conflict("진행 중인 대화의 출처는 완료 후 수정할 수 있습니다.".into()));
            }
            Some(_) => {}
        }
        let records = replace_turn_response_sources(connection, turn_id, &sources).await.map_err(|error| match error {
            SourceStoreError::Invalid(invalid) => FeedError::from(invalid),
            SourceStoreError::Database(database) => FeedError::Database(database),
        })?;
        sqlx::query("UPDATE turns SET updated_at = now(), response_source_warning = NULL WHERE id = $1::uuid")
            .bind(turn_id)
            .execute(&mut *connection)
            .await?;
        Ok(records)
    })
    .await?;
    Ok(object([("sources", Value::Array(records))]))
}

pub async fn live_feed(pool: &PgPool, cache: &TurnCostSummaryCache, params: &SearchParams) -> Result<Value, FeedError> {
    let requested = params.get("limit").map_or(120.0, string_to_number);
    let limit = js_max(20.0, js_min(requested, 300.0));
    let turns = query_turn_feed(pool, None, None, limit, 0.0, true).await?;
    let cost_summary = cache.read(pool).await?;
    Ok(object([("turns", Value::Array(turns)), ("costSummary", (*cost_summary).clone())]))
}

pub async fn archive_feed(pool: &PgPool, params: &SearchParams) -> Result<Value, FeedError> {
    let requested_limit = params.get("limit").map_or(12.0, string_to_number);
    let requested_offset = params.get("offset").map_or(0.0, string_to_number);
    let query = params.get("query").map(js_trim).filter(|query| !query.is_empty());
    let limit = js_max(1.0, js_min(requested_limit, 50.0));
    let offset = js_max(0.0, if requested_offset.is_finite() { requested_offset.floor() } else { 0.0 });
    let count = async {
        sqlx::query_scalar::<_, i32>(AssertSqlSafe(ARCHIVE_COUNT_SQL.as_str())).bind(query).fetch_optional(pool).await.map_err(FeedError::from)
    };
    let (turns, total) = tokio::try_join!(query_turn_feed(pool, None, query, limit, offset, false), count)?;
    Ok(object([("turns", Value::Array(turns)), ("total", total.unwrap_or(0).into())]))
}

pub async fn live_feed_turn(pool: &PgPool, cache: &TurnCostSummaryCache, turn_id: &str) -> Result<Value, FeedError> {
    let mut turns = query_turn_feed(pool, Some(turn_id), None, 1.0, 0.0, true).await?;
    if turns.is_empty() {
        return Err(FeedError::NotFound);
    }
    let cost_summary = cache.read(pool).await?;
    Ok(object([("turn", turns.swap_remove(0)), ("costSummary", (*cost_summary).clone())]))
}

async fn query_turn_feed(
    pool: &PgPool,
    turn_id: Option<&str>,
    query: Option<&str>,
    limit: f64,
    offset: f64,
    includes_character_minimums: bool,
) -> Result<Vec<Value>, FeedError> {
    let rows: Vec<Value> = sqlx::query_scalar(AssertSqlSafe(TURN_FEED_SQL.as_str()))
        .bind(turn_id)
        .bind(number_to_string(limit))
        .bind(query)
        .bind(number_to_string(offset))
        .bind(includes_character_minimums)
        .bind(LIVE_FEED_MINIMUM_TURNS_PER_CHARACTER)
        .fetch_all(pool)
        .await?;
    let ids: Vec<String> = rows.iter().filter_map(|turn| turn["id"].as_str().map(str::to_string)).collect();
    type Delivery = (Option<String>, Option<String>, Option<String>, Option<String>, Option<String>);
    let deliveries: Vec<Delivery> = sqlx::query_as(AssertSqlSafe(REPLY_DELIVERIES_SQL)).bind(ids).fetch_all(pool).await?;
    let mut delivery_by_turn: HashMap<String, Vec<Value>> = HashMap::new();
    for (source_turn_id, recipient_id, recipient_name, status, error) in deliveries {
        let key = source_turn_id.clone().unwrap_or_default();
        delivery_by_turn.entry(key).or_default().push(json!({
            "sourceTurnId": source_turn_id,
            "recipientId": recipient_id,
            "recipientName": recipient_name,
            "status": status,
            "error": error,
        }));
    }
    blocking(move || {
        let roots = TranscriptRoots::default();
        rows.into_iter()
            .map(|mut turn| {
                let id = turn["id"].as_str().unwrap_or_default().to_string();
                let list = delivery_by_turn.remove(&id).unwrap_or_default();
                let first = list.first().cloned().unwrap_or(Value::Null);
                if let Some(fields) = turn.as_object_mut() {
                    fields.insert("replyDeliveries".into(), Value::Array(list));
                    fields.insert("replyDelivery".into(), first);
                }
                let mut turn = with_artifact_previews(turn)?;
                with_session_context(&mut turn, &roots);
                Ok(turn)
            })
            .collect()
    })
    .await
}

/// 파일 읽기가 섞인 정리 작업은 블로킹 스레드에서 돌린다.
async fn blocking<T: Send + 'static>(work: impl FnOnce() -> Result<T, FeedError> + Send + 'static) -> Result<T, FeedError> {
    tokio::task::spawn_blocking(work).await.map_err(|join_error| FeedError::Internal(join_error.to_string()))?
}

fn string_field<'a>(turn: &'a Map<String, Value>, name: &str) -> Option<&'a str> {
    turn.get(name).and_then(Value::as_str)
}

/// `new Date(value).getTime()`. 응답의 시각은 iso() 꼴 문자열이거나 null이다(null은 0).
fn time_value(value: Option<&Value>) -> f64 {
    match value {
        Some(Value::String(text)) => date_parse(text).unwrap_or(f64::NAN),
        _ => 0.0,
    }
}

/// Node withArtifactPreviews. 응답 본문 끝에 이번 턴의 생성 이미지 미리보기를 붙인다(키 자리는 그대로).
fn with_artifact_previews(turn: Value) -> Result<Value, FeedError> {
    let session_id = turn.get("externalSessionId").and_then(Value::as_str).map(str::to_string);
    with_session_artifact_previews(turn, session_id.as_deref())
}

/// withArtifactPreviews(turn, sessionID). 세션 ID를 따로 받는 직원별 기록용이다.
fn with_session_artifact_previews(mut turn: Value, session_id: Option<&str>) -> Result<Value, FeedError> {
    let Some(fields) = turn.as_object_mut() else {
        return Ok(turn);
    };
    let started_at = time_value(fields.get("startedAt"));
    let ended_at = string_field(fields, "endedAt").map(|text| date_parse(text).unwrap_or(f64::NAN));
    let backend = string_field(fields, "backend");
    let images = generated_images_for_turn(
        session_id,
        started_at,
        ended_at,
        &generated_image_root(backend),
    )
    .map_err(|io_error| FeedError::Internal(io_error.to_string()))?;
    let response = append_local_image_previews(string_field(fields, "response").unwrap_or(""), &images);
    fields.insert("response".into(), Value::String(response));
    Ok(turn)
}

/// Node withSessionContext. 대화가 끝난 시점(진행 중이면 지금)의 컨텍스트 점유를 마지막 키로 붙인다.
fn with_session_context(turn: &mut Value, roots: &TranscriptRoots) {
    let Some(fields) = turn.as_object_mut() else {
        return;
    };
    let non_null = |name: &str| fields.get(name).filter(|value| !value.is_null());
    let backend = non_null("backend").or_else(|| non_null("characterBackend")).and_then(Value::as_str);
    let model = non_null("resolvedModel").or_else(|| non_null("model")).and_then(Value::as_str);
    let at = non_null("endedAt").map(|value| time_value(Some(value)));
    let context_window_override = if string_field(fields, "providerKind") == Some("local") {
        fields.get("providerContextWindow").and_then(Value::as_f64)
    } else {
        None
    };
    let request = ContextRequest {
        backend,
        session_id: string_field(fields, "externalSessionId"),
        model,
        at,
        context_window_override,
    };
    let usage = session_context_usage(&request, roots);
    let value = usage.map_or(Value::Null, |usage| json!({ "usedTokens": usage.used_tokens, "limitTokens": usage.limit_tokens }));
    fields.insert("sessionContext".into(), value);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 로컬_추론_강도_식은_meromero_모델_id를_넣는다() {
        assert!(LOCAL_TURN_EFFORT_SQL.contains(&format!("IN ('{MEROMERO_MODEL_ID}','{MEROMERO_26B_MODEL_ID}')")));
        assert!(LOCAL_TURN_EFFORT_SQL.contains("llama-cpp-b10982"));
    }

    #[test]
    fn 질의의_매개변수는_모든_자리에_형식을_밝힌다() {
        // 문자열로 보낸 매개변수가 형식 없이 비교되면 PG가 `uuid = text`로 거절한다.
        for sql in [TURN_FEED_SQL.as_str(), WORK_RECORDS_SQL, HISTORY_SQL.as_str(), ARCHIVE_COUNT_SQL.as_str()] {
            let bytes = sql.as_bytes();
            for (index, _) in sql.match_indices('$') {
                let rest = &sql[index + 1..];
                let digits = rest.chars().take_while(char::is_ascii_digit).count();
                if digits == 0 {
                    continue;
                }
                let after = &bytes[index + 1 + digits..];
                assert!(after.starts_with(b"::"), "형식 없는 매개변수: {}", &sql[index..(index + 12).min(sql.len())]);
            }
        }
    }

    #[test]
    fn 응답과_키_자리를_지키며_마지막에_문맥을_붙인다() {
        let turn = json!({
            "id": "t",
            "backend": "codex",
            "externalSessionId": null,
            "response": "  본문  ",
            "startedAt": "2026-10-31T00:00:00.000Z",
            "endedAt": null,
            "providerKind": "cloud",
        });
        let mut turn = with_artifact_previews(turn).unwrap();
        with_session_context(&mut turn, &TranscriptRoots::default());
        let keys: Vec<&str> = turn.as_object().unwrap().keys().map(String::as_str).collect();
        assert_eq!(keys, ["id", "backend", "externalSessionId", "response", "startedAt", "endedAt", "providerKind", "sessionContext"]);
        assert_eq!(turn["response"], "본문");
        assert_eq!(turn["sessionContext"], Value::Null);
    }
}
