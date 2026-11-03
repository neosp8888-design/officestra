// 완료 턴을 작업 기록으로 저장하고, 업무 기록 검색어를 PostgreSQL tsquery로 바꾸며, 검색 가능한 기록을 RAG 문서로 맞춘다. Node work-record-memory.mjs를 옮겼다.

use std::fmt;
use std::sync::LazyLock;

use regex::Regex;
use serde_json::{Map, Value, json};
use sqlx::{PgConnection, Row};
use unicode_normalization::UnicodeNormalization;

use crate::js::{is_js_whitespace, js_length, js_trim, utf16_prefix};
use crate::rag::database_message;

const TITLE_LIMIT: usize = 120;

/// workRecordTitle. 요청의 첫 비지 않은 줄을 공백 하나로 접고, 120자를 넘으면 119자와 말줄임표로 줄인다.
pub fn work_record_title(prompt: &str) -> String {
    let first_line = prompt.split('\n').map(js_trim).find(|line| !line.is_empty()).unwrap_or("완료된 업무");
    let mut compact = String::with_capacity(first_line.len());
    let mut in_space = false;
    for character in first_line.chars() {
        if is_js_whitespace(character) {
            if !in_space {
                compact.push(' ');
            }
            in_space = true;
        } else {
            compact.push(character);
            in_space = false;
        }
    }
    if js_length(&compact) <= TITLE_LIMIT { compact } else { format!("{}…", utf16_prefix(&compact, TITLE_LIMIT - 1)) }
}

/// completedTurnRecordBody. 사용자 요청과 최종 결과만 남긴다.
pub fn completed_turn_record_body(prompt: &str, response: &str) -> String {
    js_trim(&["요청", js_trim(prompt), "", "결과", js_trim(response)].join("\n")).to_string()
}

/// persistCompletedTurnWorkRecord의 입력. 없는 값은 Node 기본값과 같다.
#[derive(Debug, Clone, Default)]
pub struct CompletedTurnRecord<'a> {
    pub repository_root: &'a str,
    pub turn_id: &'a str,
    pub workspace_id: Option<&'a str>,
    pub character_id: &'a str,
    pub prompt: &'a str,
    pub response: &'a str,
    pub backend: Option<&'a str>,
    pub model: Option<&'a str>,
    pub needs_input: bool,
    pub response_source_count: usize,
    pub response_source_warning: Option<&'a str>,
    /// 없으면 not_applicable이다. Node 기본 매개변수처럼 빈 문자열은 그대로 저장한다.
    pub review_status: Option<&'a str>,
    pub review_tree: Option<&'a str>,
    pub head_commit: Option<&'a str>,
    /// 비지 않은 배열일 때만 review.changedFiles로 넣는다.
    pub changed_files: Option<&'a Value>,
    pub recorded_at: Option<&'a str>,
    /// CLI 후속 응답(터미널 Antigravity)만 쓰는 갱신 분기.
    pub refresh_terminal_result: bool,
}

#[derive(Debug)]
pub enum WorkRecordError {
    /// Node의 일반 Error. 문구를 그대로 올린다.
    Invalid(String),
    Database(sqlx::Error),
}

impl fmt::Display for WorkRecordError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Invalid(message) => formatter.write_str(message),
            Self::Database(error) => formatter.write_str(&database_message(error)),
        }
    }
}

impl From<sqlx::Error> for WorkRecordError {
    fn from(error: sqlx::Error) -> Self {
        Self::Database(error)
    }
}

/// path.posix.basename. 끝의 '/'는 지우고 마지막 조각을 쓴다.
fn basename(path: &str) -> &str {
    let trimmed = path.trim_end_matches('/');
    trimmed.rsplit('/').next().unwrap_or("")
}

const PERSIST_COMPLETED_TURN_SQL: &str = r#"
      WITH selected_project AS (
        INSERT INTO projects (repository_root, name)
        VALUES ($1, $2)
        ON CONFLICT (repository_root) DO UPDATE
        SET
          name = COALESCE(projects.name, EXCLUDED.name),
          updated_at = CASE
            WHEN projects.name IS NULL AND EXCLUDED.name IS NOT NULL
              THEN now()
            ELSE projects.updated_at
          END
        RETURNING id
      ), upserted_record AS (
        INSERT INTO work_records (
          project_id,
          record_type,
          lifecycle_state,
          title,
          body,
          character_id,
          attribution,
          source_turn_id,
          source_workspace_id,
          metadata,
          recorded_at,
          updated_at
        )
        SELECT
          project.id,
          'result',
          'active',
          $6,
          $7,
          $5,
          'character',
          $3::uuid,
          $4::uuid,
          $8::jsonb,
          COALESCE($9::timestamptz, now()),
          now()
        FROM selected_project AS project
        ON CONFLICT (source_turn_id, record_type)
          WHERE source_turn_id IS NOT NULL AND import_id IS NULL
        DO UPDATE SET id = work_records.id
        RETURNING *
      ), selected_record AS (
        SELECT * FROM upserted_record
        UNION ALL
        SELECT record.*
        FROM work_records AS record
        WHERE record.source_turn_id = $3::uuid
          AND record.record_type = 'result'
          AND record.import_id IS NULL
          AND NOT EXISTS (SELECT 1 FROM upserted_record)
      ), inserted_event AS (
        INSERT INTO work_record_events (
          record_id,
          record_version,
          event_type,
          actor_character_id,
          actor_type,
          source_turn_id,
          previous_value,
          next_value,
          idempotency_key,
          occurred_at
        )
        SELECT
          record.id,
          1,
          'created',
          $5,
          'character',
          $3::uuid,
          NULL,
          jsonb_build_object(
            'recordType', record.record_type,
            'lifecycleState', record.lifecycle_state,
            'title', record.title
          ),
          'completed-turn:' || $3::text,
          COALESCE($9::timestamptz, now())
        FROM selected_record AS record
        ON CONFLICT DO NOTHING
        RETURNING id
      )
      SELECT record.id::text AS "workRecordId"
      FROM selected_record AS record
"#;

const REFRESH_TERMINAL_RESULT_SQL: &str = r#"
        WITH previous AS MATERIALIZED (
          SELECT record.* FROM work_records AS record
          JOIN turns AS turn ON turn.id = record.source_turn_id
          WHERE record.source_turn_id = $1::uuid AND record.record_type = 'result'
            AND record.import_id IS NULL AND turn.origin = 'terminal'
            AND turn.backend = 'antigravity' AND turn.status = 'completed'
          FOR UPDATE OF record
        ), updated_record AS (
          UPDATE work_records AS record
          SET body = $2, metadata = record.metadata || $3::jsonb, updated_at = now()
          FROM previous
          WHERE record.id = previous.id
            AND (record.body IS DISTINCT FROM $2 OR record.metadata IS DISTINCT FROM record.metadata || $3::jsonb)
          RETURNING record.*
        )
        INSERT INTO work_record_events (
          record_id, record_version, event_type, actor_character_id, actor_type,
          source_turn_id, previous_value, next_value, occurred_at
        )
        SELECT record.id,
          COALESCE((SELECT max(event.record_version) FROM work_record_events AS event WHERE event.record_id = record.id), 0) + 1,
          'updated', $4, 'character', $1::uuid,
          jsonb_build_object('body', previous.body, 'metadata', previous.metadata),
          jsonb_build_object('body', record.body, 'metadata', record.metadata), now()
        FROM updated_record AS record JOIN previous ON previous.id = record.id
"#;

/// completed_turn 작업 기록의 metadata. 키 순서와 생략 규칙이 Node와 같다.
fn completed_turn_metadata(record: &CompletedTurnRecord<'_>) -> Value {
    let present = |value: Option<&str>| value.filter(|text| !text.is_empty()).map(str::to_string);
    let mut review = Map::new();
    review.insert("status".into(), json!(record.review_status.unwrap_or("not_applicable")));
    if let Some(tree) = present(record.review_tree) {
        review.insert("reviewTree".into(), json!(tree));
    }
    if let Some(head) = present(record.head_commit) {
        review.insert("headCommit".into(), json!(head));
    }
    if let Some(files) = record.changed_files.filter(|files| files.as_array().is_some_and(|items| !items.is_empty())) {
        review.insert("changedFiles".into(), files.clone());
    }
    let mut metadata = Map::new();
    metadata.insert("source".into(), json!("completed_turn"));
    metadata.insert("needsInput".into(), json!(record.needs_input));
    metadata.insert("responseSourceCount".into(), json!(record.response_source_count));
    if let Some(warning) = present(record.response_source_warning) {
        metadata.insert("responseSourceWarning".into(), json!(warning));
    }
    metadata.insert("review".into(), Value::Object(review));
    if let Some(backend) = present(record.backend) {
        metadata.insert("backend".into(), json!(backend));
    }
    if let Some(model) = present(record.model) {
        metadata.insert("model".into(), json!(model));
    }
    Value::Object(metadata)
}

/// persistCompletedTurnWorkRecord. 완료 턴 하나에 결과 기록 하나만 만들고(source turn 고유키), 생성 사건을 한 번 남긴다.
/// 이미 있으면 기존 기록 ID를 돌려준다. 터미널 후속 응답 갱신이면 본문·metadata를 바꾸고 갱신 사건을 더한다.
pub async fn persist_completed_turn_work_record(
    connection: &mut PgConnection,
    record: &CompletedTurnRecord<'_>,
) -> Result<Option<String>, WorkRecordError> {
    let root = js_trim(record.repository_root);
    if root.is_empty() {
        return Err(WorkRecordError::Invalid("작업 기록의 저장소 경로가 필요합니다.".into()));
    }
    let title = work_record_title(record.prompt);
    let body = completed_turn_record_body(record.prompt, record.response);
    let metadata = completed_turn_metadata(record).to_string();
    if record.refresh_terminal_result {
        sqlx::query("SELECT pg_advisory_xact_lock(hashtext($1))")
            .bind(format!("officestra:work-record:{}", record.turn_id))
            .execute(&mut *connection)
            .await?;
    }
    let name = Some(basename(root)).filter(|name| !name.is_empty());
    let work_record_id: Option<String> = sqlx::query_scalar(PERSIST_COMPLETED_TURN_SQL)
        .bind(root)
        .bind(name)
        .bind(record.turn_id)
        .bind(record.workspace_id)
        .bind(record.character_id)
        .bind(&title)
        .bind(&body)
        .bind(&metadata)
        .bind(record.recorded_at)
        .fetch_optional(&mut *connection)
        .await?;
    if record.refresh_terminal_result {
        // 일반 완료의 멱등 동작은 유지한다. CLI 후속 응답 갱신만 별도 허용해 보관함과 파생 RAG가 최초의 대기 문구에 고정되지 않게 한다.
        sqlx::query(REFRESH_TERMINAL_RESULT_SQL)
            .bind(record.turn_id)
            .bind(&body)
            .bind(&metadata)
            .bind(record.character_id)
            .execute(&mut *connection)
            .await?;
    }
    Ok(work_record_id)
}

const SEARCH_TOKEN_LIMIT: usize = 10;
const SEARCH_STOP_WORDS: [&str; 15] = [
    "그냥", "계속", "계속해", "다시", "부탁", "부탁해", "이거", "이제", "진행", "진행해", "처리", "처리해", "확인", "확인해",
    "해줘",
];
static SEARCH_TOKEN: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[\p{L}\p{N}_]+").unwrap());

/// workRecordSearchTSQuery. NFKC·소문자로 바꾼 단어 최대 10개를 접두 일치 OR로 잇는다.
pub fn work_record_search_ts_query(query: &str) -> String {
    let normalized = query.nfkc().collect::<String>().to_lowercase();
    let mut unique: Vec<&str> = Vec::new();
    for token in SEARCH_TOKEN.find_iter(&normalized).map(|found| found.as_str()) {
        let length = js_length(token);
        if !(2..=64).contains(&length) || SEARCH_STOP_WORDS.contains(&token) || unique.contains(&token) {
            continue;
        }
        unique.push(token);
        if unique.len() >= SEARCH_TOKEN_LIMIT {
            break;
        }
    }
    unique.iter().map(|token| format!("{token}:*")).collect::<Vec<_>>().join(" | ")
}

pub struct RagDocumentSync {
    pub changed: usize,
    pub deleted: u64,
    pub documents: Vec<Value>,
}

const DELETE_UNSEARCHABLE_SQL: &str = r#"
      DELETE FROM rag_documents AS document
      USING work_records AS record, projects AS project
      WHERE document.work_record_id = record.id
        AND project.id = record.project_id
        AND ($1::text IS NULL OR project.repository_root = $1::text)
        AND ($2::uuid IS NULL OR record.id = $2::uuid)
        AND NOT EXISTS (
          SELECT 1
          FROM searchable_work_record_ids AS searchable
          WHERE searchable.id = record.id
        )
"#;

const UPSERT_DOCUMENTS_SQL: &str = r#"
      WITH desired_documents AS (
        SELECT
          record.id AS work_record_id,
          record.title,
          COALESCE(
            NULLIF(record.body, ''),
            NULLIF(items.content, ''),
            ''
          ) AS content,
          jsonb_strip_nulls(jsonb_build_object(
            'workRecordId', record.id::text,
            'projectId', record.project_id::text,
            'recordType', record.record_type,
            'lifecycleState', record.lifecycle_state,
            'characterId', record.character_id,
            'attribution', record.attribution,
            'recordedAt', record.recorded_at,
            'sourcePath', record.source_path,
            'sourceCommit', record.source_commit
          )) AS metadata
        FROM work_records AS record
        JOIN searchable_work_record_ids AS searchable
          ON searchable.id = record.id
        JOIN projects AS project ON project.id = record.project_id
        LEFT JOIN LATERAL (
          SELECT string_agg(
            CASE
              WHEN item.is_checked IS TRUE THEN '- [x] ' || item.item_text
              WHEN item.is_checked IS FALSE THEN '- [ ] ' || item.item_text
              ELSE '- ' || item.item_text
            END,
            E'\n' ORDER BY item.ordinal
          ) AS content
          FROM work_record_items AS item
          WHERE item.record_id = record.id
        ) AS items ON true
        WHERE ($1::text IS NULL OR project.repository_root = $1::text)
          AND ($2::uuid IS NULL OR record.id = $2::uuid)
      )
      INSERT INTO rag_documents (
        source,
        title,
        content,
        metadata,
        work_record_id,
        updated_at
      )
      SELECT
        'work_record',
        desired.title,
        desired.content,
        desired.metadata,
        desired.work_record_id,
        now()
      FROM desired_documents AS desired
      ON CONFLICT (work_record_id) WHERE work_record_id IS NOT NULL
      DO UPDATE
      SET
        source = EXCLUDED.source,
        title = EXCLUDED.title,
        content = EXCLUDED.content,
        metadata = EXCLUDED.metadata,
        embedding = NULL,
        embedding_model = NULL,
        embedding_updated_at = NULL,
        embedding_error = NULL,
        updated_at = now()
      WHERE (
        rag_documents.source,
        rag_documents.title,
        rag_documents.content,
        rag_documents.metadata
      ) IS DISTINCT FROM (
        EXCLUDED.source,
        EXCLUDED.title,
        EXCLUDED.content,
        EXCLUDED.metadata
      )
      RETURNING json_build_object(
        'ragDocumentId', id::text,
        'workRecordId', work_record_id::text
      )
"#;

/// syncWorkRecordRAGDocuments. 검색에서 빠진 기록의 문서는 지우고, 바뀐 기록만 문서를 다시 써 임베딩을 비운다.
pub async fn sync_work_record_rag_documents(
    connection: &mut PgConnection,
    repository_root: Option<&str>,
    work_record_id: Option<&str>,
) -> Result<RagDocumentSync, sqlx::Error> {
    let deleted = sqlx::query(DELETE_UNSEARCHABLE_SQL)
        .bind(repository_root)
        .bind(work_record_id)
        .execute(&mut *connection)
        .await?
        .rows_affected();
    let documents: Vec<Value> = sqlx::query_scalar(UPSERT_DOCUMENTS_SQL)
        .bind(repository_root)
        .bind(work_record_id)
        .fetch_all(&mut *connection)
        .await?;
    Ok(RagDocumentSync { changed: documents.len(), deleted, documents })
}

/// transitionTurnWorkRecordReview의 입력. 빈 문자열 커밋·tree는 Node처럼 review에 넣지 않는다.
#[derive(Debug, Clone, Default)]
pub struct ReviewTransition {
    pub turn_id: String,
    pub status: String,
    pub task_commit: Option<String>,
    pub merged_commit: Option<String>,
    pub review_tree: Option<String>,
    pub head_commit: Option<String>,
    /// 배열일 때만 review.changedFiles로 넣는다.
    pub changed_files: Option<Value>,
    pub error_message: Option<String>,
    /// 없으면 Node 기본값 user다.
    pub actor_type: Option<String>,
}

const TRANSITION_REVIEW_SQL: &str = r#"
      WITH updated_record AS (
        UPDATE work_records
        SET
          lifecycle_state = CASE WHEN $2 = 'rejected' THEN 'archived' ELSE 'active' END,
          metadata = jsonb_set(metadata, '{review}', COALESCE(metadata->'review', '{}'::jsonb) || $3::jsonb, true),
          updated_at = now()
        WHERE source_turn_id = $1::uuid
          AND record_type = 'result'
          AND import_id IS NULL
        RETURNING *
      ), inserted_event AS (
        INSERT INTO work_record_events (
          record_id, record_version, event_type, actor_character_id, actor_type,
          source_turn_id, previous_value, next_value, idempotency_key, occurred_at
        )
        SELECT
          record.id,
          COALESCE((SELECT max(event.record_version) FROM work_record_events AS event WHERE event.record_id = record.id), 0) + 1,
          'state_changed',
          NULL,
          $4,
          $1::uuid,
          NULL,
          jsonb_build_object('lifecycleState', record.lifecycle_state, 'review', record.metadata->'review'),
          $5,
          now()
        FROM updated_record AS record
        ON CONFLICT (record_id, idempotency_key) WHERE idempotency_key IS NOT NULL DO NOTHING
        RETURNING id
      )
      SELECT id::text FROM updated_record
"#;

/// transitionTurnWorkRecordReview. 턴의 결과 기록에 검토 상태를 합치고 상태 변경 사건을 한 번만 남긴다.
/// 거절이면 기록을 보관 상태로 돌린다. 바뀐 기록 ID를 돌려준다.
pub async fn transition_turn_work_record_review(
    connection: &mut PgConnection,
    transition: &ReviewTransition,
) -> Result<Option<String>, sqlx::Error> {
    let present = |value: &Option<String>| value.as_deref().filter(|text| !text.is_empty()).map(str::to_string);
    let mut review = serde_json::Map::new();
    review.insert("status".into(), Value::String(transition.status.clone()));
    for (key, value) in [
        ("taskCommit", &transition.task_commit),
        ("mergedCommit", &transition.merged_commit),
        ("reviewTree", &transition.review_tree),
        ("headCommit", &transition.head_commit),
    ] {
        if let Some(text) = present(value) {
            review.insert(key.into(), Value::String(text));
        }
    }
    if let Some(changed_files) = transition.changed_files.as_ref().filter(|files| files.is_array()) {
        review.insert("changedFiles".into(), changed_files.clone());
    }
    review.insert("errorMessage".into(), transition.error_message.clone().map_or(Value::Null, Value::String));
    let event_key_part = present(&transition.task_commit)
        .or_else(|| present(&transition.merged_commit))
        .or_else(|| present(&transition.review_tree))
        .unwrap_or_else(|| "none".into());
    sqlx::query("SELECT pg_advisory_xact_lock(hashtext($1))")
        .bind(format!("officestra:work-record:{}", transition.turn_id))
        .execute(&mut *connection)
        .await?;
    sqlx::query_scalar(TRANSITION_REVIEW_SQL)
        .bind(&transition.turn_id)
        .bind(&transition.status)
        .bind(Value::Object(review).to_string())
        .bind(transition.actor_type.as_deref().unwrap_or("user"))
        .bind(format!("review:{}:{event_key_part}", transition.status))
        .fetch_optional(&mut *connection)
        .await
}

const TERMINAL_CANDIDATES_SQL: &str = r#"
      WITH terminal_candidates AS (
        SELECT
          record.source_turn_id::text AS "turnID",
          workspace.status,
          workspace.task_commit AS "taskCommit",
          workspace.merged_commit AS "mergedCommit",
          workspace.review_tree AS "reviewTree",
          workspace.head_commit AS "headCommit",
          workspace.changed_files AS "changedFiles",
          record.lifecycle_state AS "lifecycleState",
          record.metadata #>> '{review,status}' AS "reviewStatus",
          record.recorded_at AS "recordedAt",
          record.id AS "recordID"
        FROM work_records AS record
        JOIN projects AS project
          ON project.id = record.project_id
        JOIN task_workspaces AS workspace
          ON workspace.id = record.source_workspace_id
          AND workspace.review_turn_id = record.source_turn_id
        WHERE record.record_type = 'result'
          AND record.import_id IS NULL
          AND record.source_turn_id IS NOT NULL
          AND workspace.status IN ('merged', 'rejected')
          AND ($1::text IS NULL OR project.repository_root = $1)

        UNION ALL

        SELECT
          record.source_turn_id::text AS "turnID",
          'not_required' AS status,
          NULL AS "taskCommit",
          NULL AS "mergedCommit",
          workspace.review_tree AS "reviewTree",
          workspace.head_commit AS "headCommit",
          workspace.changed_files AS "changedFiles",
          record.lifecycle_state AS "lifecycleState",
          record.metadata #>> '{review,status}' AS "reviewStatus",
          record.recorded_at AS "recordedAt",
          record.id AS "recordID"
        FROM work_records AS record
        JOIN projects AS project
          ON project.id = record.project_id
        JOIN task_workspaces AS workspace
          ON workspace.id = record.source_workspace_id
        JOIN LATERAL (
          SELECT turn.id
          FROM turns AS turn
          WHERE turn.task_workspace_id = workspace.id
            AND turn.status = 'completed'
          ORDER BY turn.started_at DESC, turn.id DESC
          LIMIT 1
        ) AS latest_turn
          ON latest_turn.id = record.source_turn_id
        WHERE record.record_type = 'result'
          AND record.import_id IS NULL
          AND record.source_turn_id IS NOT NULL
          AND record.lifecycle_state = 'active'
          AND record.metadata->>'needsInput' IS DISTINCT FROM 'true'
          AND record.metadata #>> '{review,status}'
            IN ('awaiting_approval', 'conflict')
          AND workspace.status = 'closed'
          AND workspace.task_commit IS NULL
          AND workspace.merged_commit IS NULL
          AND workspace.merged_at IS NULL
          AND workspace.rejected_at IS NULL
          AND workspace.review_tree IS NULL
          AND jsonb_array_length(workspace.changed_files) = 0
          AND ($1::text IS NULL OR project.repository_root = $1)
      )
      SELECT *
      FROM terminal_candidates AS candidate
      WHERE (
          candidate.status = 'rejected'
          AND (
            candidate."lifecycleState" IS DISTINCT FROM 'archived'
            OR candidate."reviewStatus" IS DISTINCT FROM 'rejected'
          )
        )
        OR (
          candidate.status IN ('merged', 'not_required')
          AND (
            candidate."lifecycleState" IS DISTINCT FROM 'active'
            OR candidate."reviewStatus" IS DISTINCT FROM candidate.status
          )
        )
      ORDER BY candidate."recordedAt", candidate."recordID"
"#;

/// reconcileTerminalWorkRecordReviews. 통합·거절·변경 없이 닫힌 작업 공간과 어긋난 결과 기록의 검토 상태를 시스템 행위자로 맞춘다.
/// 바꾼 기록 ID를 돌려준다.
pub async fn reconcile_terminal_work_record_reviews(
    connection: &mut PgConnection,
    repository_root: Option<&str>,
) -> Result<Vec<String>, sqlx::Error> {
    let candidates = sqlx::query(TERMINAL_CANDIDATES_SQL).bind(repository_root).fetch_all(&mut *connection).await?;
    let mut work_record_ids = Vec::new();
    for candidate in candidates {
        let transition = ReviewTransition {
            turn_id: candidate.try_get("turnID")?,
            status: candidate.try_get("status")?,
            task_commit: candidate.try_get("taskCommit")?,
            merged_commit: candidate.try_get("mergedCommit")?,
            review_tree: candidate.try_get("reviewTree")?,
            head_commit: candidate.try_get("headCommit")?,
            changed_files: Some(candidate.try_get::<Option<Value>, _>("changedFiles")?.unwrap_or_else(|| json!([]))),
            error_message: None,
            actor_type: Some("system".into()),
        };
        if let Some(work_record_id) = transition_turn_work_record_review(connection, &transition).await? {
            work_record_ids.push(work_record_id);
        }
    }
    Ok(work_record_ids)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 검색어는_정규화한_단어의_접두_일치_or가_된다() {
        // Node v24 workRecordSearchTSQuery 결과.
        assert_eq!(work_record_search_ts_query("메모리 부족으로 중단"), "메모리:* | 부족으로:* | 중단:*");
        assert_eq!(work_record_search_ts_query("ＲＡＭ 95% 확인해 계속 a ComfyUI ComfyUI"), "ram:* | 95:* | comfyui:*");
        assert_eq!(work_record_search_ts_query("  !!  "), "");
        assert_eq!(work_record_search_ts_query("일1 이2 삼3 사4 오5 육6 칠7 팔8 구9 십10 열하나"), "일1:* | 이2:* | 삼3:* | 사4:* | 오5:* | 육6:* | 칠7:* | 팔8:* | 구9:* | 십10:*");
        assert_eq!(work_record_search_ts_query("snake_case 𝐀𝐁"), "snake_case:* | ab:*");
    }

    #[test]
    fn 완료_턴_제목과_본문은_node와_같다() {
        // Node v24 workRecordTitle·completedTurnRecordBody 결과. 홀로 남은 대리 문자는 node-pg 전송처럼 U+FFFD로 본다.
        for (prompt, expected) in [
            ("  세션 유지 상태를 확인해줘  \n추가 설명", "세션 유지 상태를 확인해줘".to_string()),
            ("\n\n   \n두번째 줄\t탭  여러   공백", "두번째 줄 탭 여러 공백".into()),
            ("", "완료된 업무".into()),
            ("   \n  ", "완료된 업무".into()),
            ("\r\n윈도 줄\r\n", "윈도 줄".into()),
            ("\u{a0}\u{2028}  줄 구분자 포함\u{2029}다음\u{feff}", "줄 구분자 포함 다음".into()),
        ] {
            assert_eq!(work_record_title(prompt), expected, "{prompt:?}");
        }
        assert_eq!(work_record_title(&"가".repeat(120)), "가".repeat(120));
        assert_eq!(work_record_title(&"가".repeat(121)), format!("{}…", "가".repeat(119)));
        assert_eq!(work_record_title(&format!("{}😀끝", "가".repeat(118))), format!("{}\u{fffd}…", "가".repeat(118)));
        assert_eq!(work_record_title(&format!("{}😀끝끝", "가".repeat(117))), format!("{}😀…", "가".repeat(117)));
        for (prompt, response, expected) in [
            ("  세션을 확인해줘.  ", "\n문제없습니다.\n", "요청\n세션을 확인해줘.\n\n결과\n문제없습니다."),
            ("", "", "요청\n\n\n결과"),
            ("요청만", "  ", "요청\n요청만\n\n결과"),
            ("\u{3000}전각\u{3000}", "결과\r\n", "요청\n전각\n\n결과\n결과"),
        ] {
            assert_eq!(completed_turn_record_body(prompt, response), expected);
        }
    }

    #[test]
    fn 완료_턴_metadata는_node와_키_순서와_생략이_같다() {
        let changed = json!([{ "path": "a", "status": "M" }]);
        let empty = json!([]);
        let base = CompletedTurnRecord { repository_root: "/repo", turn_id: "t", character_id: "boss", ..Default::default() };
        // Node v24 persistCompletedTurnWorkRecord가 보낸 $8 원문.
        for (record, expected) in [
            (
                CompletedTurnRecord { review_status: Some("not_applicable"), ..base.clone() },
                r#"{"source":"completed_turn","needsInput":false,"responseSourceCount":0,"review":{"status":"not_applicable"}}"#,
            ),
            (
                CompletedTurnRecord {
                    needs_input: true,
                    response_source_count: 2,
                    response_source_warning: Some("출처 경고"),
                    review_status: Some("needs_input"),
                    backend: Some("codex"),
                    model: Some("gpt-5.6-sol"),
                    ..base.clone()
                },
                r#"{"source":"completed_turn","needsInput":true,"responseSourceCount":2,"responseSourceWarning":"출처 경고","review":{"status":"needs_input"},"backend":"codex","model":"gpt-5.6-sol"}"#,
            ),
            (
                CompletedTurnRecord {
                    review_status: Some("awaiting_approval"),
                    review_tree: Some("tree-1"),
                    head_commit: Some("head-1"),
                    changed_files: Some(&changed),
                    backend: Some(""),
                    model: Some(""),
                    ..base.clone()
                },
                r#"{"source":"completed_turn","needsInput":false,"responseSourceCount":0,"review":{"status":"awaiting_approval","reviewTree":"tree-1","headCommit":"head-1","changedFiles":[{"path":"a","status":"M"}]}}"#,
            ),
            (
                CompletedTurnRecord {
                    review_status: Some(""),
                    changed_files: Some(&empty),
                    response_source_warning: Some(""),
                    ..base.clone()
                },
                r#"{"source":"completed_turn","needsInput":false,"responseSourceCount":0,"review":{"status":""}}"#,
            ),
        ] {
            assert_eq!(completed_turn_metadata(&record).to_string(), expected);
        }
        assert_eq!(completed_turn_metadata(&base)["review"]["status"], "not_applicable", "검토 상태가 없으면 Node 기본값");
    }
}
