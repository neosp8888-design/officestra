// 승인형 사내 위키의 페이지 조회와 제안 수명주기. Node wiki-knowledge.mjs를 옮겼고 응답 키·문구·상태 코드가 같다.

use std::sync::LazyLock;

use serde_json::{Value, json};
use sqlx::{AssertSqlSafe, PgConnection, PgExecutor};

use crate::js::{
    js_max, js_min, js_string_or_empty, js_to_number, js_trim, js_truthy, number_to_string, pg_text, string_to_number,
    to_js_json,
};
use crate::turn_feed::iso;
use crate::work_record_memory::{sync_work_record_rag_documents, work_record_search_ts_query};

pub const WIKI_PROPOSAL_STATES: [&str; 7] =
    ["candidate", "drafted", "peer_verified", "pending_user", "published", "rejected", "conflict"];
pub const WIKI_APPROVAL_GRADES: [&str; 3] = ["auto", "peer", "user"];
pub const WIKI_PROPOSAL_KINDS: [&str; 3] = ["decision", "constraint", "incident"];
const TERMINAL_STATES: [&str; 3] = ["published", "rejected", "conflict"];
const MAX_TITLE_LENGTH: usize = 120;
const MAX_BODY_LENGTH: usize = 12_000;

#[derive(Debug)]
pub enum WikiError {
    /// WikiKnowledgeError. 상태 코드와 문구를 그대로 응답한다.
    Knowledge { status: u16, message: String },
    Database(sqlx::Error),
}

impl WikiError {
    pub fn status(&self) -> u16 {
        match self {
            Self::Knowledge { status, .. } => *status,
            Self::Database(_) => 500,
        }
    }

    pub fn message(&self) -> String {
        match self {
            Self::Knowledge { message, .. } => message.clone(),
            Self::Database(error) => match error.as_database_error() {
                Some(database_error) => database_error.message().to_string(),
                None => error.to_string(),
            },
        }
    }

    /// isWikiPageKeyRaceError. 같은 pageKey 동시 게시로 유니크 제약이 터진 경우다.
    pub fn is_page_key_race(&self) -> bool {
        let Self::Database(error) = self else { return false };
        error.as_database_error().is_some_and(|database_error| {
            database_error.code().as_deref() == Some("23505")
                && database_error.constraint().unwrap_or("").contains("synthesis_page_key")
        })
    }
}

impl From<sqlx::Error> for WikiError {
    fn from(error: sqlx::Error) -> Self {
        Self::Database(error)
    }
}

pub type WikiResult<T> = Result<T, WikiError>;

fn invalid(message: impl Into<String>, status: u16) -> WikiError {
    WikiError::Knowledge { status, message: message.into() }
}

/// wiki-knowledge.mjs의 UUID_PATTERN. 판·변형 자리를 따지지 않는다.
fn is_loose_uuid(value: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.len() == 36
        && bytes.iter().enumerate().all(|(index, byte)| match index {
            8 | 13 | 18 | 23 => *byte == b'-',
            _ => byte.is_ascii_hexdigit(),
        })
}

/// normalizedWikiPageKey. 영문 소문자·숫자로 시작하는 1~80자 슬러그다.
pub fn normalized_wiki_page_key(value: Option<&Value>) -> WikiResult<String> {
    let text = js_string_or_empty(value);
    let normalized = js_trim(&text);
    let bytes = normalized.as_bytes();
    let valid = !bytes.is_empty()
        && bytes.len() <= 80
        && matches!(bytes[0], b'a'..=b'z' | b'0'..=b'9')
        && bytes.iter().all(|byte| matches!(byte, b'a'..=b'z' | b'0'..=b'9' | b'-'));
    if !valid {
        return Err(invalid("pageKey는 영문 소문자·숫자로 시작하는 1~80자의 슬러그여야 합니다.", 400));
    }
    Ok(normalized.to_string())
}

fn normalized_proposal_kind(value: Option<&Value>) -> WikiResult<String> {
    let text = js_string_or_empty(value);
    let kind = js_trim(&text);
    if !WIKI_PROPOSAL_KINDS.contains(&kind) {
        return Err(invalid(format!("알 수 없는 제안 종류입니다. {kind}"), 400));
    }
    Ok(kind.to_string())
}

fn normalized_draft_text(title: Option<&Value>, body: Option<&Value>) -> WikiResult<(String, String)> {
    let title = js_string_or_empty(title);
    let body = js_string_or_empty(body);
    let (title, body) = (js_trim(&title), js_trim(&body));
    if title.is_empty() || title.chars().count() > MAX_TITLE_LENGTH {
        return Err(invalid("제안 제목은 1~120자여야 합니다.", 400));
    }
    if body.chars().count() > MAX_BODY_LENGTH {
        return Err(invalid("제안 본문은 12,000자 이하여야 합니다.", 400));
    }
    Ok((title.to_string(), body.to_string()))
}

/// normalizedSourceWorkRecordIDs. UUID만 받아 소문자로 바꾸고 순서를 지켜 중복을 뺀다.
pub fn normalized_source_work_record_ids(value: Option<&Value>, required: bool) -> WikiResult<Vec<String>> {
    let entries = match value {
        None | Some(Value::Null) => {
            if required {
                return Err(invalid("source workRecord ID가 최소 1개 필요합니다.", 400));
            }
            return Ok(Vec::new());
        }
        Some(Value::Array(entries)) => entries,
        Some(_) => return Err(invalid("sourceWorkRecordIds는 배열이어야 합니다.", 400)),
    };
    let mut unique: Vec<String> = Vec::new();
    for entry in entries {
        let id = js_trim(&js_string_or_empty(Some(entry))).to_lowercase();
        if !is_loose_uuid(&id) {
            return Err(invalid("sourceWorkRecordIds에 올바르지 않은 UUID가 있습니다.", 400));
        }
        if !unique.contains(&id) {
            unique.push(id);
        }
    }
    if required && unique.is_empty() {
        return Err(invalid("source workRecord ID가 최소 1개 필요합니다.", 400));
    }
    Ok(unique)
}

/// wikiProposalActionOutcome의 입력. actor_type 기본값은 "character"다.
pub struct ActionInput<'a> {
    pub state: &'a str,
    pub approval_grade: &'a str,
    pub action: &'a str,
    pub actor_type: &'a str,
    pub author_character_id: Option<&'a str>,
    pub actor_character_id: Option<&'a Value>,
}

/// 상태 전이 규칙의 단일 판정 지점. 다음 상태를 돌려주고 위반은 오류다.
pub fn wiki_proposal_action_outcome(input: &ActionInput<'_>) -> WikiResult<&'static str> {
    if !WIKI_PROPOSAL_STATES.contains(&input.state) {
        return Err(invalid(format!("알 수 없는 제안 상태입니다. {}", input.state), 400));
    }
    if !WIKI_APPROVAL_GRADES.contains(&input.approval_grade) {
        return Err(invalid(format!("알 수 없는 승인 등급입니다. {}", input.approval_grade), 400));
    }
    match input.action {
        "reject" => {
            if TERMINAL_STATES.contains(&input.state) {
                return Err(invalid("이미 종결된 제안은 거절할 수 없습니다.", 409));
            }
            Ok("rejected")
        }
        "verify" => {
            if input.state != "drafted" {
                return Err(invalid("drafted 상태의 제안만 검증할 수 있습니다.", 409));
            }
            if input.approval_grade == "auto" {
                return Err(invalid("auto 등급 제안은 검증 없이 승인합니다.", 409));
            }
            if !js_truthy(input.actor_character_id) {
                return Err(invalid("검증에는 characterId가 필요합니다.", 400));
            }
            let same_author = matches!(
                (input.actor_character_id, input.author_character_id),
                (Some(Value::String(actor)), Some(author)) if actor == author
            );
            if input.approval_grade == "peer" && same_author {
                return Err(invalid("peer 등급은 작성자가 스스로 검증할 수 없습니다.", 403));
            }
            Ok(if input.approval_grade == "user" { "pending_user" } else { "peer_verified" })
        }
        "approve" => {
            if input.approval_grade == "auto" {
                if input.state != "drafted" {
                    return Err(invalid("auto 등급은 drafted 상태에서만 승인합니다.", 409));
                }
                return Ok("published");
            }
            if input.approval_grade == "peer" {
                if input.state != "peer_verified" {
                    return Err(invalid("peer 등급은 검증 완료 후에만 승인합니다.", 409));
                }
                return Ok("published");
            }
            if input.state != "pending_user" {
                return Err(invalid("user 등급은 pending_user 상태에서만 승인합니다.", 409));
            }
            if input.actor_type != "user" {
                return Err(invalid("user 등급 제안은 사용자만 승인할 수 있습니다.", 403));
            }
            Ok("published")
        }
        action => Err(invalid(format!("알 수 없는 동작입니다. {action}"), 400)),
    }
}

/// proposalDTO와 같은 키 순서의 JSON. 시각은 node-pg Date의 toISOString 꼴이다.
static PROPOSAL_DTO: LazyLock<String> = LazyLock::new(|| {
    format!(
        "json_build_object(
          'id', proposal.id,
          'projectId', proposal.project_id,
          'pageKey', proposal.page_key,
          'targetRecordId', proposal.target_record_id,
          'baseVersion', proposal.base_version,
          'state', proposal.state,
          'kind', proposal.proposal_kind,
          'approvalTier', proposal.approval_grade,
          'title', proposal.draft_title,
          'body', proposal.draft_body,
          'sourceRecordIds', CASE
            WHEN jsonb_typeof(proposal.source_work_record_ids) = 'array' THEN proposal.source_work_record_ids
            ELSE '[]'::jsonb
          END,
          'authorCharacterId', proposal.author_character_id,
          'authorTurnId', proposal.author_turn_id,
          'verifierCharacterId', proposal.verifier_character_id,
          'reason', proposal.reason,
          'ordinal', proposal.ordinal,
          'createdAt', {},
          'updatedAt', {},
          'verifiedAt', {},
          'resolvedAt', {}
        )",
        iso("proposal.created_at"),
        iso("proposal.updated_at"),
        iso("proposal.verified_at"),
        iso("proposal.resolved_at"),
    )
});

const RECORD_VERSION_SQL: &str = "COALESCE(
          (
            SELECT MAX(event.record_version)
            FROM work_record_events AS event
            WHERE event.record_id = record.id
          ),
          0
        )";

/// PAGE_SELECT. 최상위 시각만 toISOString 꼴로 바꿨다.
static PAGE_SELECT: LazyLock<String> = LazyLock::new(|| {
    format!(
        r#"
  SELECT
    record.id,
    record.project_id AS "projectId",
    project.name AS "projectName",
    record.metadata->>'pageKey' AS "pageKey",
    record.metadata->>'kind' AS kind,
    record.title,
    record.body,
    record.character_id AS "characterId",
    {} AS "recordedAt",
    {} AS "updatedAt",
    {RECORD_VERSION_SQL} AS version,
    COALESCE((
      SELECT json_agg(
        json_build_object(
          'workRecordId', source.id,
          'title', source.title,
          'excerpt', left(
            regexp_replace(COALESCE(source.body, ''), '\s+', ' ', 'g'),
            240
          )
        )
        ORDER BY link.created_at, source.id
      )
      FROM work_record_links AS link
      JOIN work_records AS source ON source.id = link.target_record_id
      WHERE link.source_record_id = record.id
        AND link.relation = 'derived_from'
    ), '[]'::json) AS sources
  FROM work_records AS record
  JOIN projects AS project ON project.id = record.project_id
"#,
        iso("record.recorded_at"),
        iso("record.updated_at"),
    )
});

/// listWikiPages. query가 있으면 위키 전용 검색 뷰에서 순위로, 없으면 최근 수정 순으로 돌려준다.
pub async fn list_wiki_pages(
    executor: impl PgExecutor<'_>,
    query: Option<&str>,
    limit: Option<&str>,
    repository_root: Option<&str>,
) -> WikiResult<Vec<Value>> {
    let requested = limit.map_or(f64::NAN, string_to_number);
    let fallback = if requested.is_nan() || requested == 0.0 { 20.0 } else { requested };
    let bounded = number_to_string(js_min(js_max(fallback, 1.0), 100.0));
    if let Some(query) = query.filter(|query| !query.is_empty()) {
        let ts_query = work_record_search_ts_query(query);
        if ts_query.is_empty() {
            return Ok(Vec::new());
        }
        let sql = format!(
            "SELECT row_to_json(page) FROM (
        {}
        JOIN searchable_wiki_page_documents AS document
          ON document.work_record_id = record.id
        WHERE document.search_document @@ to_tsquery('simple', $1::text)
          AND ($2::text IS NULL OR project.repository_root = $2::text)
        ORDER BY ts_rank(document.search_document, to_tsquery('simple', $1::text)) DESC,
          record.updated_at DESC
        LIMIT $3::bigint
      ) AS page",
            *PAGE_SELECT
        );
        return Ok(sqlx::query_scalar(AssertSqlSafe(sql))
            .bind(ts_query)
            .bind(repository_root)
            .bind(bounded)
            .fetch_all(executor)
            .await?);
    }
    let sql = format!(
        "SELECT row_to_json(page) FROM (
      {}
      WHERE record.record_type = 'synthesis'
        AND record.lifecycle_state = 'active'
        AND ($1::text IS NULL OR project.repository_root = $1::text)
      ORDER BY record.updated_at DESC
      LIMIT $2::bigint
    ) AS page",
        *PAGE_SELECT
    );
    Ok(sqlx::query_scalar(AssertSqlSafe(sql)).bind(repository_root).bind(bounded).fetch_all(executor).await?)
}

/// getWikiPage. 활성 위키 페이지 하나, 없으면 None이다.
pub async fn get_wiki_page(
    executor: impl PgExecutor<'_>,
    page_id: &str,
    repository_root: Option<&str>,
) -> WikiResult<Option<Value>> {
    let sql = format!(
        "SELECT row_to_json(page) FROM (
      {}
      WHERE record.id = $1::uuid
        AND record.record_type = 'synthesis'
        AND record.lifecycle_state = 'active'
        AND ($2::text IS NULL OR project.repository_root = $2::text)
    ) AS page",
        *PAGE_SELECT
    );
    Ok(sqlx::query_scalar(AssertSqlSafe(sql)).bind(page_id).bind(repository_root).fetch_optional(executor).await?)
}

/// archiveWikiPage. 행은 지우지 않고 archived로 바꾸고 상태 변경 이력을 남긴다.
pub async fn archive_wiki_page(
    connection: &mut PgConnection,
    page_id: &str,
    repository_root: Option<&str>,
) -> WikiResult<Value> {
    let locked: Option<(String, i32)> = sqlx::query_as(AssertSqlSafe(format!(
        "SELECT
        record.lifecycle_state,
        {RECORD_VERSION_SQL} AS version
      FROM work_records AS record
      JOIN projects AS project ON project.id = record.project_id
      WHERE record.id = $1::uuid
        AND record.record_type = 'synthesis'
        AND ($2::text IS NULL OR project.repository_root = $2::text)
      FOR UPDATE OF record"
    )))
    .bind(page_id)
    .bind(repository_root)
    .fetch_optional(&mut *connection)
    .await?;
    let Some((_, version)) = locked.filter(|(state, _)| state == "active") else {
        return Err(invalid("위키 페이지를 찾을 수 없습니다.", 404));
    };
    sqlx::query(
        "UPDATE work_records
      SET lifecycle_state = 'archived',
        updated_at = now()
      WHERE id = $1::uuid",
    )
    .bind(page_id)
    .execute(&mut *connection)
    .await?;
    sqlx::query(
        "INSERT INTO work_record_events (
        record_id,
        record_version,
        event_type,
        actor_type,
        previous_value,
        next_value
      )
      VALUES ($1::uuid, $2::integer, 'state_changed', 'user', $3::jsonb, $4::jsonb)",
    )
    .bind(page_id)
    .bind(version + 1)
    .bind(r#"{"lifecycleState":"active"}"#)
    .bind(r#"{"lifecycleState":"archived"}"#)
    .execute(&mut *connection)
    .await?;
    Ok(json!({ "id": page_id, "lifecycleState": "archived" }))
}

/// listWikiProposals. 최근 갱신 순 200개다.
pub async fn list_wiki_proposals(
    executor: impl PgExecutor<'_>,
    state: Option<&str>,
    repository_root: Option<&str>,
) -> WikiResult<Vec<Value>> {
    if let Some(state) = state.filter(|state| !WIKI_PROPOSAL_STATES.contains(state)) {
        return Err(invalid(format!("알 수 없는 제안 상태입니다. {state}"), 400));
    }
    let sql = format!(
        "SELECT {}
      FROM wiki_proposals AS proposal
      JOIN projects AS project ON project.id = proposal.project_id
      WHERE ($1::text IS NULL OR proposal.state = $1::text)
        AND ($2::text IS NULL OR project.repository_root = $2::text)
      ORDER BY proposal.updated_at DESC
      LIMIT 200",
        *PROPOSAL_DTO
    );
    Ok(sqlx::query_scalar(AssertSqlSafe(sql)).bind(state).bind(repository_root).fetch_all(executor).await?)
}

async fn resolve_project_id(
    connection: &mut PgConnection,
    project_id: Option<&str>,
    repository_root: Option<&str>,
) -> WikiResult<String> {
    let (sql, value) = match (project_id, repository_root) {
        (Some(project_id), _) => ("SELECT id::text FROM projects WHERE id = $1::uuid", project_id),
        (None, Some(root)) => ("SELECT id::text FROM projects WHERE repository_root = $1::text", root),
        (None, None) => return Err(invalid("projectId 또는 repositoryRoot가 필요합니다.", 400)),
    };
    let found: Option<String> = sqlx::query_scalar(sql).bind(value).fetch_optional(connection).await?;
    found.ok_or_else(|| invalid("프로젝트를 찾을 수 없습니다.", 404))
}

#[derive(sqlx::FromRow)]
struct LockedPage {
    id: String,
    title: Option<String>,
    body: Option<String>,
    version: i32,
}

async fn lock_wiki_page(connection: &mut PgConnection, project_id: &str, page_key: &str) -> WikiResult<Option<LockedPage>> {
    Ok(sqlx::query_as(AssertSqlSafe(format!(
        "SELECT
        record.id::text AS id,
        record.title,
        record.body,
        {RECORD_VERSION_SQL} AS version
      FROM work_records AS record
      WHERE record.project_id = $1::uuid
        AND record.record_type = 'synthesis'
        AND record.metadata->>'pageKey' = $2::text
      FOR UPDATE OF record"
    )))
    .bind(project_id)
    .bind(page_key)
    .fetch_optional(connection)
    .await?)
}

/// 근거 검증은 게시의 유일한 사실 관문이다. 없는 기록·다른 프로젝트·위키 자기참조를 거부한다.
async fn assert_publishable_sources(
    connection: &mut PgConnection,
    project_id: &str,
    source_ids: &[String],
    require_searchable: bool,
) -> WikiResult<()> {
    if source_ids.is_empty() {
        return Err(invalid("근거 workRecord 없이 게시할 수 없습니다.", 422));
    }
    let rows: Vec<(String, String, String, bool)> = sqlx::query_as(
        "SELECT
        record.id::text,
        record.project_id::text,
        record.record_type,
        EXISTS (
          SELECT 1
          FROM searchable_work_record_ids AS searchable
          WHERE searchable.id = record.id
        ) AS searchable
      FROM work_records AS record
      WHERE record.id = ANY($1::uuid[])",
    )
    .bind(source_ids)
    .fetch_all(connection)
    .await?;
    for source_id in source_ids {
        let Some((_, owner, record_type, searchable)) = rows.iter().find(|row| &row.0 == source_id) else {
            return Err(invalid(format!("근거 workRecord가 없습니다. {source_id}"), 422));
        };
        if owner != project_id {
            return Err(invalid(format!("다른 프로젝트의 workRecord는 근거가 될 수 없습니다. {source_id}"), 422));
        }
        if record_type == "synthesis" {
            return Err(invalid(format!("위키 페이지는 위키의 근거가 될 수 없습니다. {source_id}"), 422));
        }
        if require_searchable && !searchable {
            return Err(invalid(format!("승인·병합되지 않은 작업 기록은 게시 근거가 될 수 없습니다. {source_id}"), 422));
        }
    }
    Ok(())
}

/// createWikiProposal의 입력. Value 필드는 JS 쪽에서 아무 JSON이나 올 수 있는 값이다.
pub struct NewProposal<'a> {
    pub project_id: Option<&'a str>,
    pub repository_root: Option<&'a str>,
    pub page_key: Option<&'a Value>,
    pub approval_grade: &'a str,
    pub state: &'a str,
    pub kind: Option<&'a Value>,
    pub ordinal: Option<&'a Value>,
    pub draft_title: Option<&'a Value>,
    pub draft_body: Option<&'a Value>,
    pub source_work_record_ids: Option<&'a Value>,
    pub author_character_id: Option<&'a str>,
    pub author_turn_id: Option<&'a str>,
    pub base_version: Option<&'a Value>,
}

fn non_negative_integer(value: f64) -> bool {
    value.is_finite() && value.fract() == 0.0 && value >= 0.0
}

/// createWikiProposal. 검증 순서와 문구가 Node와 같다. 같은 턴·순번 제안은 기존 행을 돌려준다.
pub async fn create_wiki_proposal(connection: &mut PgConnection, input: NewProposal<'_>) -> WikiResult<Value> {
    if !["candidate", "drafted", "pending_user"].contains(&input.state) {
        return Err(invalid("생성 상태는 candidate, drafted 또는 pending_user만 허용합니다.", 400));
    }
    if !WIKI_APPROVAL_GRADES.contains(&input.approval_grade) {
        return Err(invalid(format!("알 수 없는 승인 등급입니다. {}", input.approval_grade), 400));
    }
    if input.state == "pending_user" && input.approval_grade != "user" {
        return Err(invalid("pending_user 상태는 user 승인 등급만 허용합니다.", 400));
    }
    if input.approval_grade == "peer" && input.author_character_id.is_none_or(str::is_empty) {
        return Err(invalid("peer 등급 제안에는 authorCharacterId가 필요합니다.", 400));
    }
    let page_key = normalized_wiki_page_key(input.page_key)?;
    let kind = normalized_proposal_kind(input.kind)?;
    let (title, body) = normalized_draft_text(input.draft_title, input.draft_body)?;
    let ordinal = match input.ordinal.filter(|value| !value.is_null()) {
        None => None,
        Some(value) => {
            let number = js_to_number(value);
            if !non_negative_integer(number) {
                return Err(invalid("ordinal은 0 이상의 정수여야 합니다.", 400));
            }
            Some(number_to_string(number))
        }
    };
    let sources = normalized_source_work_record_ids(input.source_work_record_ids, input.state != "candidate")?;
    let project_id = resolve_project_id(connection, input.project_id, input.repository_root).await?;
    if !sources.is_empty() {
        assert_publishable_sources(connection, &project_id, &sources, false).await?;
    }
    let page: Option<(String, i32)> = sqlx::query_as(AssertSqlSafe(format!(
        "SELECT
        record.id::text,
        {RECORD_VERSION_SQL} AS version
      FROM work_records AS record
      WHERE record.project_id = $1::uuid
        AND record.record_type = 'synthesis'
        AND record.metadata->>'pageKey' = $2::text"
    )))
    .bind(&project_id)
    .bind(&page_key)
    .fetch_optional(&mut *connection)
    .await?;
    let (target_record_id, current_version) = match page {
        Some((id, version)) => (Some(id), version),
        None => (None, 0),
    };
    let requested_base = match input.base_version.filter(|value| !value.is_null()) {
        None => f64::from(current_version),
        Some(value) => js_to_number(value),
    };
    if !non_negative_integer(requested_base) {
        return Err(invalid("baseVersion은 0 이상의 정수여야 합니다.", 400));
    }
    let sql = format!(
        "WITH proposal AS (
      INSERT INTO wiki_proposals (
        project_id,
        page_key,
        target_record_id,
        base_version,
        state,
        approval_grade,
        proposal_kind,
        ordinal,
        draft_title,
        draft_body,
        source_work_record_ids,
        author_character_id,
        author_turn_id
      )
      VALUES (
        $1::uuid, $2::text, $3::uuid, $4::integer, $5::text, $6::text, $7::text, $8::integer, $9::text, $10::text,
        $11::jsonb, $12::text, $13::uuid
      )
      ON CONFLICT (author_turn_id, ordinal)
        WHERE author_turn_id IS NOT NULL AND ordinal IS NOT NULL
      DO UPDATE SET id = wiki_proposals.id
      RETURNING *
    )
    SELECT {} FROM proposal",
        *PROPOSAL_DTO
    );
    let sources_json = String::from_utf8(to_js_json(&sources).unwrap_or_default()).unwrap_or_default();
    Ok(sqlx::query_scalar(AssertSqlSafe(sql))
        .bind(&project_id)
        .bind(&page_key)
        .bind(target_record_id)
        .bind(number_to_string(requested_base))
        .bind(input.state)
        .bind(input.approval_grade)
        .bind(&kind)
        .bind(ordinal)
        .bind(&title)
        .bind(&body)
        .bind(sources_json)
        .bind(input.author_character_id)
        .bind(input.author_turn_id)
        .fetch_one(connection)
        .await?)
}

#[derive(sqlx::FromRow)]
struct LockedProposal {
    project_id: String,
    page_key: String,
    state: String,
    approval_grade: String,
    proposal_kind: String,
    base_version: i32,
    draft_title: String,
    draft_body: String,
    source_work_record_ids: Value,
    author_character_id: Option<String>,
    author_turn_id: Option<String>,
}

async fn lock_wiki_proposal(
    connection: &mut PgConnection,
    proposal_id: &str,
    repository_root: Option<&str>,
) -> WikiResult<LockedProposal> {
    let locked = sqlx::query_as(
        "SELECT
        proposal.project_id::text AS project_id,
        proposal.page_key,
        proposal.state,
        proposal.approval_grade,
        proposal.proposal_kind,
        proposal.base_version,
        proposal.draft_title,
        proposal.draft_body,
        proposal.source_work_record_ids,
        proposal.author_character_id,
        proposal.author_turn_id::text AS author_turn_id
      FROM wiki_proposals AS proposal
      JOIN projects AS project ON project.id = proposal.project_id
      WHERE proposal.id = $1::uuid
        AND ($2::text IS NULL OR project.repository_root = $2::text)
      FOR UPDATE OF proposal",
    )
    .bind(proposal_id)
    .bind(repository_root)
    .fetch_optional(connection)
    .await?;
    locked.ok_or_else(|| invalid("제안을 찾을 수 없습니다.", 404))
}

/// UPDATE ... RETURNING * 결과를 proposalDTO로 돌려준다.
async fn update_proposal(
    connection: &mut PgConnection,
    assignments: &str,
    proposal_id: &str,
    values: &[Option<String>],
) -> WikiResult<Value> {
    let sql = format!(
        "WITH proposal AS (
      UPDATE wiki_proposals
      SET {assignments}
      WHERE id = $1::uuid
      RETURNING *
    )
    SELECT {} FROM proposal",
        *PROPOSAL_DTO
    );
    let mut query = sqlx::query_scalar(AssertSqlSafe(sql)).bind(proposal_id);
    for value in values {
        query = query.bind(value.clone());
    }
    Ok(query.fetch_one(connection).await?)
}

/// verifyWikiProposal. characterId는 요청 본문의 아무 JSON 값이며 node-pg처럼 텍스트로 저장한다.
pub async fn verify_wiki_proposal(
    connection: &mut PgConnection,
    proposal_id: &str,
    verifier_character_id: Option<&Value>,
    repository_root: Option<&str>,
) -> WikiResult<Value> {
    let proposal = lock_wiki_proposal(connection, proposal_id, repository_root).await?;
    let next_state = wiki_proposal_action_outcome(&ActionInput {
        state: &proposal.state,
        approval_grade: &proposal.approval_grade,
        action: "verify",
        actor_type: "character",
        author_character_id: proposal.author_character_id.as_deref(),
        actor_character_id: verifier_character_id,
    })?;
    update_proposal(
        connection,
        "state = $2::text,
        verifier_character_id = $3::text,
        verified_at = now(),
        updated_at = now()",
        proposal_id,
        &[Some(next_state.into()), verifier_character_id.and_then(pg_text)],
    )
    .await
}

/// rejectWikiProposal. reason도 요청 본문의 아무 JSON 값이다.
pub async fn reject_wiki_proposal(
    connection: &mut PgConnection,
    proposal_id: &str,
    reason: Option<&Value>,
    repository_root: Option<&str>,
) -> WikiResult<Value> {
    let proposal = lock_wiki_proposal(connection, proposal_id, repository_root).await?;
    wiki_proposal_action_outcome(&ActionInput {
        state: &proposal.state,
        approval_grade: &proposal.approval_grade,
        action: "reject",
        actor_type: "character",
        author_character_id: None,
        actor_character_id: None,
    })?;
    update_proposal(
        connection,
        "state = 'rejected',
        reason = $2::text,
        resolved_at = now(),
        updated_at = now()",
        proposal_id,
        &[reason.and_then(pg_text)],
    )
    .await
}

async fn mark_proposal_conflict(connection: &mut PgConnection, proposal_id: &str, reason: String) -> WikiResult<Value> {
    update_proposal(
        connection,
        "state = 'conflict',
        reason = $2::text,
        resolved_at = now(),
        updated_at = now()",
        proposal_id,
        &[Some(reason)],
    )
    .await
}

pub struct ApproveOutcome {
    pub proposal: Value,
    pub conflicted: bool,
}

/// approveWikiProposal. 전이 판정과 게시를 한 트랜잭션에서 끝내고, 기준 버전이 다르면 conflict로 종결한다.
pub async fn approve_wiki_proposal(
    connection: &mut PgConnection,
    proposal_id: &str,
    actor_type: &str,
    actor_character_id: Option<&str>,
    repository_root: Option<&str>,
) -> WikiResult<ApproveOutcome> {
    let proposal = lock_wiki_proposal(connection, proposal_id, repository_root).await?;
    let actor = actor_character_id.map(|id| Value::String(id.into()));
    wiki_proposal_action_outcome(&ActionInput {
        state: &proposal.state,
        approval_grade: &proposal.approval_grade,
        action: "approve",
        actor_type,
        author_character_id: proposal.author_character_id.as_deref(),
        actor_character_id: actor.as_ref(),
    })?;
    let sources = normalized_source_work_record_ids(Some(&proposal.source_work_record_ids), true)?;
    assert_publishable_sources(connection, &proposal.project_id, &sources, true).await?;

    let page = lock_wiki_page(connection, &proposal.project_id, &proposal.page_key).await?;
    let current_version = page.as_ref().map_or(0, |page| page.version);
    if current_version != proposal.base_version {
        let conflicted = mark_proposal_conflict(
            connection,
            proposal_id,
            format!("기준 버전 {}이(가) 현재 버전 {current_version}과 다릅니다.", proposal.base_version),
        )
        .await?;
        return Ok(ApproveOutcome { proposal: conflicted, conflicted: true });
    }

    let page_id = match &page {
        Some(page) => {
            sqlx::query(
                "UPDATE work_records
        SET title = $2::text,
          body = $3::text,
          lifecycle_state = 'active',
          metadata = metadata || jsonb_build_object(
            'pageKey', $4::text,
            'kind', $5::text
          ),
          updated_at = now()
        WHERE id = $1::uuid",
            )
            .bind(&page.id)
            .bind(&proposal.draft_title)
            .bind(&proposal.draft_body)
            .bind(&proposal.page_key)
            .bind(&proposal.proposal_kind)
            .execute(&mut *connection)
            .await?;
            page.id.clone()
        }
        None => {
            sqlx::query_scalar(
                "INSERT INTO work_records (
          project_id,
          record_type,
          lifecycle_state,
          title,
          body,
          character_id,
          attribution,
          metadata
        )
        VALUES ($1::uuid, 'synthesis', 'active', $2::text, $3::text, $4::text, 'character',
          jsonb_build_object(
            'pageKey', $5::text,
            'kind', $6::text
          ))
        RETURNING id::text",
            )
            .bind(&proposal.project_id)
            .bind(&proposal.draft_title)
            .bind(&proposal.draft_body)
            .bind(&proposal.author_character_id)
            .bind(&proposal.page_key)
            .bind(&proposal.proposal_kind)
            .fetch_one(&mut *connection)
            .await?
        }
    };

    let previous = page.as_ref().map(|page| json!({ "title": page.title, "body": page.body }).to_string());
    sqlx::query(
        "INSERT INTO work_record_events (
        record_id,
        record_version,
        event_type,
        actor_character_id,
        actor_type,
        source_turn_id,
        previous_value,
        next_value
      )
      VALUES ($1::uuid, $2::integer, $3::text, $4::text, $5::text, $6::uuid, $7::jsonb, $8::jsonb)",
    )
    .bind(&page_id)
    .bind(current_version + 1)
    .bind(if page.is_none() { "created" } else { "updated" })
    .bind(actor_character_id)
    .bind(actor_type)
    .bind(&proposal.author_turn_id)
    .bind(previous)
    .bind(json!({ "title": proposal.draft_title, "body": proposal.draft_body }).to_string())
    .execute(&mut *connection)
    .await?;

    sqlx::query(
        "DELETE FROM work_record_links
      WHERE source_record_id = $1::uuid
        AND relation = 'derived_from'",
    )
    .bind(&page_id)
    .execute(&mut *connection)
    .await?;
    for source_id in &sources {
        sqlx::query(
            "INSERT INTO work_record_links (
          source_record_id,
          target_record_id,
          relation
        )
        VALUES ($1::uuid, $2::uuid, 'derived_from')
        ON CONFLICT DO NOTHING",
        )
        .bind(&page_id)
        .bind(source_id)
        .execute(&mut *connection)
        .await?;
    }

    sync_work_record_rag_documents(connection, None, Some(&page_id)).await?;

    let published = update_proposal(
        connection,
        "state = 'published',
        target_record_id = $2::uuid,
        resolved_at = now(),
        updated_at = now()",
        proposal_id,
        &[Some(page_id)],
    )
    .await?;
    Ok(ApproveOutcome { proposal: published, conflicted: false })
}

/// resolveWikiPageKeyRace. 동시 게시 경합 뒤 새 트랜잭션에서 제안을 conflict로 종결한다.
pub async fn resolve_wiki_page_key_race(
    connection: &mut PgConnection,
    proposal_id: &str,
    repository_root: Option<&str>,
) -> WikiResult<Value> {
    lock_wiki_proposal(connection, proposal_id, repository_root).await?;
    mark_proposal_conflict(connection, proposal_id, "같은 pageKey의 페이지가 동시에 게시됐습니다.".into()).await
}

/// 완료 턴의 위키 수정안 저장 입력. Node agent-runtime.mjs persistTurnWikiProposals의 인자와 같다.
pub struct TurnWikiProposals<'a> {
    pub repository_root: &'a str,
    pub work_record_id: Option<&'a str>,
    pub turn_id: &'a str,
    pub character_id: &'a str,
    pub proposals: Option<&'a Value>,
    pub parser_warning: Option<String>,
    pub needs_input: bool,
}

/// persistTurnWikiProposals. 확인 요청 답변이거나 작업 기록·수정안이 없으면 해석 경고만 돌려준다.
/// 수정안마다 SAVEPOINT를 두어 하나가 실패해도 나머지와 완료 저장은 이어 가고, 실패 사유는 경고 문구로 모은다.
pub async fn persist_turn_wiki_proposals(connection: &mut PgConnection, input: TurnWikiProposals<'_>) -> Result<Option<String>, sqlx::Error> {
    let mut warnings: Vec<String> = input.parser_warning.into_iter().collect();
    let proposals = match (input.needs_input, input.work_record_id.filter(|id| !id.is_empty()), input.proposals) {
        (false, Some(work_record_id), Some(Value::Array(proposals))) if !proposals.is_empty() => Some((work_record_id, proposals)),
        _ => None,
    };
    let Some((work_record_id, proposals)) = proposals else {
        return Ok((!warnings.is_empty()).then(|| warnings.join(" ")));
    };
    let source_work_record_ids = json!([work_record_id]);
    for (ordinal, proposal) in proposals.iter().enumerate() {
        let savepoint = format!("wiki_proposal_{ordinal}");
        sqlx::query(AssertSqlSafe(format!("SAVEPOINT {savepoint}"))).execute(&mut *connection).await?;
        let tier = proposal.get("approvalTier").and_then(Value::as_str).unwrap_or("");
        let ordinal_value = json!(ordinal);
        let created = create_wiki_proposal(
            connection,
            NewProposal {
                project_id: None,
                repository_root: Some(input.repository_root),
                page_key: proposal.get("pageKey"),
                approval_grade: tier,
                state: if tier == "user" { "pending_user" } else { "drafted" },
                kind: proposal.get("kind"),
                ordinal: Some(&ordinal_value),
                draft_title: proposal.get("title"),
                draft_body: proposal.get("body"),
                source_work_record_ids: Some(&source_work_record_ids),
                author_character_id: Some(input.character_id),
                author_turn_id: Some(input.turn_id),
                base_version: None,
            },
        )
        .await;
        if let Err(wiki_error) = created {
            sqlx::query(AssertSqlSafe(format!("ROLLBACK TO SAVEPOINT {savepoint}"))).execute(&mut *connection).await?;
            warnings.push(format!("위키 수정안 {}번을 저장하지 못했습니다: {}", ordinal + 1, wiki_error.message()));
        }
        sqlx::query(AssertSqlSafe(format!("RELEASE SAVEPOINT {savepoint}"))).execute(&mut *connection).await?;
    }
    Ok((!warnings.is_empty()).then(|| warnings.join(" ")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn outcome(state: &str, grade: &str, action: &str, actor_type: &str, author: Option<&str>, actor: Option<&str>) -> WikiResult<&'static str> {
        let actor = actor.map(|id| Value::String(id.into()));
        wiki_proposal_action_outcome(&ActionInput {
            state,
            approval_grade: grade,
            action,
            actor_type,
            author_character_id: author,
            actor_character_id: actor.as_ref(),
        })
    }

    fn status(result: WikiResult<&'static str>) -> u16 {
        result.unwrap_err().status()
    }

    #[test]
    fn 페이지_키는_파서와_같은_소문자_슬러그만_허용한다() {
        assert_eq!(normalized_wiki_page_key(Some(&json!("  design-decisions  "))).unwrap(), "design-decisions");
        for bad in ["   ", "!!!", "Design-Decisions", "보안-사건", "a_b"] {
            assert!(normalized_wiki_page_key(Some(&json!(bad))).is_err(), "{bad}");
        }
        assert!(normalized_wiki_page_key(Some(&json!("a".repeat(81)))).is_err());
        assert!(normalized_wiki_page_key(None).is_err());
    }

    #[test]
    fn 근거_id_목록은_uuid만_허용하고_중복을_제거한다() {
        let id = "3f2b8c1a-4d5e-4f60-8a71-92b3c4d5e6f7";
        assert_eq!(normalized_source_work_record_ids(Some(&json!([id, id.to_uppercase()])), true).unwrap(), [id]);
        assert!(normalized_source_work_record_ids(Some(&Value::Null), false).unwrap().is_empty());
        assert!(normalized_source_work_record_ids(None, true).is_err());
        assert!(normalized_source_work_record_ids(Some(&json!(["잘못된-값"])), true).is_err());
        assert!(normalized_source_work_record_ids(Some(&json!("not-array")), false).is_err());
    }

    #[test]
    fn 전이_판정표_검증은_drafted에서만_등급별_다음_상태가_다르다() {
        assert_eq!(outcome("drafted", "peer", "verify", "character", Some("author"), Some("reviewer")).unwrap(), "peer_verified");
        assert_eq!(outcome("drafted", "user", "verify", "character", None, Some("reviewer")).unwrap(), "pending_user");
        assert_eq!(status(outcome("drafted", "peer", "verify", "character", Some("author"), Some("author"))), 403);
        assert_eq!(status(outcome("drafted", "auto", "verify", "character", None, Some("reviewer"))), 409);
        assert_eq!(status(outcome("peer_verified", "peer", "verify", "character", None, Some("reviewer"))), 409);
    }

    #[test]
    fn 전이_판정표_승인은_등급별_허용_상태가_고정된다() {
        assert_eq!(outcome("drafted", "auto", "approve", "character", None, None).unwrap(), "published");
        assert_eq!(outcome("peer_verified", "peer", "approve", "character", None, None).unwrap(), "published");
        assert_eq!(outcome("pending_user", "user", "approve", "user", None, None).unwrap(), "published");
        assert_eq!(status(outcome("drafted", "user", "approve", "user", None, None)), 409);
        assert_eq!(status(outcome("pending_user", "user", "approve", "character", None, None)), 403);
        assert_eq!(status(outcome("drafted", "peer", "approve", "character", None, None)), 409);
    }

    #[test]
    fn 전이_판정표_거절은_종결_전_상태에서만_가능하다() {
        for state in ["candidate", "drafted", "peer_verified", "pending_user"] {
            assert_eq!(outcome(state, "peer", "reject", "character", None, None).unwrap(), "rejected");
        }
        for state in ["published", "rejected", "conflict"] {
            assert_eq!(status(outcome(state, "peer", "reject", "character", None, None)), 409);
        }
    }

    #[test]
    fn 검증자는_js_참거짓으로_판정한다() {
        // `!actorCharacterID`: 빈 문자열·0·false는 없음과 같고, 숫자 1은 있음이다.
        for falsy in [json!(""), json!(0), json!(false)] {
            let input = ActionInput { state: "drafted", approval_grade: "user", action: "verify", actor_type: "character", author_character_id: None, actor_character_id: Some(&falsy) };
            assert_eq!(wiki_proposal_action_outcome(&input).unwrap_err().message(), "검증에는 characterId가 필요합니다.");
        }
        let number = json!(1);
        let input = ActionInput { state: "drafted", approval_grade: "peer", action: "verify", actor_type: "character", author_character_id: Some("1"), actor_character_id: Some(&number) };
        assert_eq!(wiki_proposal_action_outcome(&input).unwrap(), "peer_verified");
    }
}
