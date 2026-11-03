// 업무 기록 조회 조건과 응답 출처 입력을 검증·정규화하고, 턴의 응답 출처를 저장한다. Node work-record-provenance.mjs를 옮겼다.

use std::collections::HashSet;
use std::fmt;
use std::sync::LazyLock;

use regex::Regex;
use serde_json::{Map, Value, json};
use sqlx::PgConnection;
use url::Url;

use crate::js::{
    SearchParams, decode_uri_component, js_length, js_string, js_string_or_empty, js_trim, relative_path, string_to_number,
};

const RECORD_TYPES: [&str; 7] = ["task", "decision", "constraint", "evidence", "status", "result", "note"];
const LIFECYCLE_STATES: [&str; 4] = ["legacy", "active", "superseded", "archived"];
const ATTRIBUTIONS: [&str; 5] = ["character", "user", "commit", "unknown", "system"];
const SOURCE_KINDS: [&str; 6] = ["rag", "database", "file", "web", "tool", "skill"];
static TOOL_LOCATOR: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[A-Za-z0-9._:-]+(?:/[A-Za-z0-9._:-]+)*$").unwrap());
static SKILL_LOCATOR: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[A-Za-z0-9._:-]+$").unwrap());
const SENSITIVE_QUERY_KEYS: [&str; 18] = [
    "key", "sig", "token", "secret", "password", "passwd", "auth", "authorization", "signature", "credential",
    "credentials", "accesskey", "privatekey", "secretkey", "authcode", "authorizationcode", "oauthcode", "code",
];
const SENSITIVE_QUERY_SUFFIXES: [&str; 8] =
    ["token", "apikey", "secret", "password", "passwd", "signature", "credential", "credentials"];

#[derive(Debug, PartialEq)]
pub struct ProvenanceValidationError(pub String);

impl fmt::Display for ProvenanceValidationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

#[derive(Debug, PartialEq)]
pub struct WorkRecordFilters {
    pub project_id: Option<String>,
    pub record_type: Option<String>,
    pub lifecycle_state: Option<String>,
    pub attribution: Option<String>,
    pub query: Option<String>,
    /// JS Number 그대로 둔다. SQL에는 String(number)로 넘긴다.
    pub limit: f64,
    pub offset: f64,
}

/// `/^[0-9a-f]{8}-[0-9a-f]{4}-[1-8][0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/i`
pub fn is_uuid(value: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.len() == 36
        && bytes.iter().enumerate().all(|(index, byte)| match index {
            8 | 13 | 18 | 23 => *byte == b'-',
            14 => (b'1'..=b'8').contains(byte),
            19 => matches!(byte.to_ascii_lowercase(), b'8' | b'9' | b'a' | b'b'),
            _ => byte.is_ascii_hexdigit(),
        })
}

pub fn parse_work_record_filters(params: &SearchParams) -> Result<WorkRecordFilters, ProvenanceValidationError> {
    let project_id = optional_uuid(params.get("projectId"), "projectId")?;
    let record_type = optional_choice(params.get("kind"), &RECORD_TYPES, "kind")?;
    let lifecycle_state = optional_choice(params.get("state"), &LIFECYCLE_STATES, "state")?;
    let attribution = optional_choice(params.get("attribution"), &ATTRIBUTIONS, "attribution")?;
    let query = optional_text(params.get("q"), 500, "q")?;
    let limit = bounded_integer(params.get("limit"), "limit", 50.0, 1.0, 100.0)?;
    let offset = bounded_integer(params.get("offset"), "offset", 0.0, 0.0, 100_000.0)?;
    Ok(WorkRecordFilters { project_id, record_type, lifecycle_state, attribution, query, limit, offset })
}

fn optional_choice(value: Option<&str>, choices: &[&str], field: &str) -> Result<Option<String>, ProvenanceValidationError> {
    let text = js_trim(value.unwrap_or(""));
    if text.is_empty() {
        return Ok(None);
    }
    if !choices.contains(&text) {
        return Err(ProvenanceValidationError(format!("{field} 값이 올바르지 않습니다.")));
    }
    Ok(Some(text.to_string()))
}

fn optional_text(value: Option<&str>, maximum: usize, field: &str) -> Result<Option<String>, ProvenanceValidationError> {
    let text = js_trim(value.unwrap_or(""));
    if text.is_empty() {
        return Ok(None);
    }
    if js_length(text) > maximum {
        return Err(ProvenanceValidationError(format!("{field} 값은 {maximum}자 이하여야 합니다.")));
    }
    Ok(Some(text.to_string()))
}

fn optional_uuid(value: Option<&str>, field: &str) -> Result<Option<String>, ProvenanceValidationError> {
    let text = js_trim(value.unwrap_or(""));
    if text.is_empty() {
        return Ok(None);
    }
    if !is_uuid(text) {
        return Err(ProvenanceValidationError(format!("{field} 값은 UUID여야 합니다.")));
    }
    Ok(Some(text.to_ascii_lowercase()))
}

fn bounded_integer(value: Option<&str>, field: &str, fallback: f64, minimum: f64, maximum: f64) -> Result<f64, ProvenanceValidationError> {
    let Some(value) = value.filter(|value| !value.is_empty()) else {
        return Ok(fallback);
    };
    let number = string_to_number(value);
    // Number.isInteger: 유한하고 소수부가 없다.
    if !number.is_finite() || number.trunc() != number || number < minimum || number > maximum {
        return Err(ProvenanceValidationError(format!("{field} 값은 {minimum} 이상 {maximum} 이하 정수여야 합니다.")));
    }
    Ok(number)
}

fn invalid<T>(message: impl Into<String>) -> Result<T, ProvenanceValidationError> {
    Err(ProvenanceValidationError(message.into()))
}

/// normalizeResponseSources. 직원 응답의 출처 목록을 저장 형식으로 바꾸고, 규칙을 어기면 그 사유를 오류로 돌려준다.
pub fn normalize_response_sources(value: &Value, maximum: usize) -> Result<Vec<Value>, ProvenanceValidationError> {
    let Value::Array(sources) = value else {
        return invalid("출처 목록은 배열이어야 합니다.");
    };
    if sources.len() > maximum {
        return invalid(format!("출처는 한 응답에 {maximum}개까지 저장할 수 있습니다."));
    }
    let mut identities = HashSet::new();
    let mut normalized = Vec::with_capacity(sources.len());
    for (ordinal, source) in sources.iter().enumerate() {
        let Value::Object(source) = source else {
            return invalid("각 출처는 객체여야 합니다.");
        };
        let field = |key: &str| source.get(key).filter(|value| !value.is_null());
        let source_kind = required_choice(field("sourceKind").or_else(|| field("kind")), &SOURCE_KINDS, "sourceKind")?;
        let title = required_text(field("title"), 200, "title")?;
        let locator = normalized_source_locator(&source_kind, required_text(field("locator"), 2_000, "locator")?)?;
        let excerpt = match field("excerpt") {
            None => None,
            Some(excerpt) => optional_text(Some(&js_string(excerpt)), 1_000, "excerpt")?,
        };
        let rag_document_id = optional_uuid(Some(&js_string_or_empty(field("ragDocumentId"))), "ragDocumentId")?;
        let work_record_id = optional_uuid(Some(&js_string_or_empty(field("workRecordId"))), "workRecordId")?;
        if source_kind == "rag" && rag_document_id.is_none() {
            return invalid("RAG 출처에는 ragDocumentId가 필요합니다.");
        }
        if rag_document_id.is_some() && source_kind != "rag" {
            return invalid("ragDocumentId는 RAG 출처에만 사용할 수 있습니다.");
        }
        if work_record_id.is_some() && source_kind != "database" {
            return invalid("workRecordId는 DB 출처에만 사용할 수 있습니다.");
        }
        if source_kind == "database"
            && locator.strip_prefix("work_records").is_some_and(|rest| rest.starts_with(['/', ':']))
            && work_record_id.is_none()
        {
            return invalid("work_records 출처에는 workRecordId가 필요합니다.");
        }
        let metadata = field("metadata").cloned().unwrap_or_else(|| Value::Object(Map::new()));
        if !metadata.is_object() {
            return invalid("metadata는 객체여야 합니다.");
        }
        if !identities.insert(format!("{source_kind}\n{locator}")) {
            return invalid("같은 출처가 중복됐습니다.");
        }
        normalized.push(json!({
            "ordinal": ordinal,
            "sourceKind": source_kind,
            "title": title,
            "locator": locator,
            "excerpt": excerpt,
            "ragDocumentID": rag_document_id,
            "workRecordID": work_record_id,
            "metadata": metadata,
        }));
    }
    Ok(normalized)
}

fn required_choice(value: Option<&Value>, choices: &[&str], field: &str) -> Result<String, ProvenanceValidationError> {
    let text = js_string_or_empty(value);
    let text = js_trim(&text);
    if !choices.contains(&text) {
        return invalid(format!("{field} 값이 올바르지 않습니다."));
    }
    Ok(text.to_string())
}

fn required_text(value: Option<&Value>, maximum: usize, field: &str) -> Result<String, ProvenanceValidationError> {
    let text = js_string_or_empty(value);
    let text = js_trim(&text);
    if text.is_empty() || js_length(text) > maximum {
        return invalid(format!("{field} 값은 1자 이상 {maximum}자 이하여야 합니다."));
    }
    Ok(text.to_string())
}

fn normalized_source_locator(source_kind: &str, locator: String) -> Result<String, ProvenanceValidationError> {
    match source_kind {
        "web" => normalized_web_locator(&locator),
        "tool" => normalized_identifier_locator(locator, &TOOL_LOCATOR, "도구 출처 locator는 도구 식별자만 사용할 수 있습니다."),
        "skill" => normalized_identifier_locator(locator, &SKILL_LOCATOR, "스킬 출처 locator는 등록된 스킬 이름만 사용할 수 있습니다."),
        _ => Ok(locator),
    }
}

/// WHATWG URL로 읽어 사용자 정보·민감한 쿼리와 fragment 키가 없을 때만 직렬화한 href를 돌려준다.
fn normalized_web_locator(locator: &str) -> Result<String, ProvenanceValidationError> {
    let Ok(url) = Url::parse(locator) else {
        return invalid("웹 출처 locator는 올바른 URL이어야 합니다.");
    };
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none_or(str::is_empty)
        || !url.username().is_empty()
        || url.password().is_some_and(|password| !password.is_empty())
    {
        return invalid("웹 출처 locator는 사용자 정보가 없는 http 또는 https URL이어야 합니다.");
    }
    if url.query_pairs().any(|(key, _)| is_sensitive_query_key(&key)) {
        return invalid("웹 출처 locator에 민감한 쿼리 키를 넣을 수 없습니다.");
    }
    // url.hash는 빈 fragment면 ""다. decodeURIComponent가 실패하면 원문을 쓰고, URLSearchParams는 맨 앞 '?'를 지운다.
    let fragment = url.fragment().unwrap_or("");
    let decoded = decode_uri_component(fragment).unwrap_or_else(|| fragment.to_string());
    let parameters = decoded.strip_prefix('?').unwrap_or(&decoded);
    if url::form_urlencoded::parse(parameters.as_bytes()).any(|(key, value)| !value.is_empty() && is_sensitive_query_key(&key)) {
        return invalid("웹 출처 locator에 민감한 fragment 키를 넣을 수 없습니다.");
    }
    Ok(url.to_string())
}

fn normalized_identifier_locator(locator: String, pattern: &Regex, message: &str) -> Result<String, ProvenanceValidationError> {
    if !pattern.is_match(&locator) || locator.split('/').any(|part| part == "." || part == "..") {
        return invalid(message);
    }
    Ok(locator)
}

fn is_sensitive_query_key(key: &str) -> bool {
    let compact: String =
        key.to_lowercase().chars().filter(|character| character.is_ascii_lowercase() || character.is_ascii_digit()).collect();
    SENSITIVE_QUERY_KEYS.contains(&compact.as_str()) || SENSITIVE_QUERY_SUFFIXES.iter().any(|suffix| compact.ends_with(suffix))
}

/// `/^(.*)(:\d+(?:-\d+)?)$/`. JS의 `.`은 줄 끝 문자를 빼고 `\d`는 ASCII 숫자다.
static LINE_SUFFIX: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^([^\n\r\u{2028}\u{2029}]*)(:[0-9]+(?:-[0-9]+)?)$").unwrap());

/// portableResponseSources. 업무 폴더 안의 절대 경로 파일 출처를 병합 뒤에도 쓸 상대 경로로 바꾸고,
/// 바꾼 뒤 겹친 출처는 처음 것만 남겨 순번을 다시 매긴다.
pub fn portable_response_sources(sources: &[Value], workdir: &str) -> Vec<Value> {
    let base_directory = js_trim(workdir);
    if base_directory.is_empty() {
        return sources.to_vec();
    }
    let converted = sources.iter().map(|source| {
        if source.get("sourceKind").and_then(Value::as_str) != Some("file") {
            return source.clone();
        }
        let locator = source.get("locator").and_then(Value::as_str).unwrap_or("");
        let (path, line_suffix) = match LINE_SUFFIX.captures(locator) {
            Some(found) => (found.get(1).map_or("", |part| part.as_str()), found.get(2).map_or("", |part| part.as_str())),
            None => (locator, ""),
        };
        if !path.starts_with('/') {
            return source.clone();
        }
        let relative = relative_path(base_directory, path);
        if relative.is_empty() || relative.starts_with('/') || relative == ".." || relative.starts_with("../") {
            return source.clone();
        }
        let mut portable = source.clone();
        portable["locator"] = json!(format!("{relative}{line_suffix}"));
        portable
    });
    let mut identities = HashSet::new();
    let mut unique: Vec<Value> = Vec::new();
    for mut source in converted {
        let kind = source.get("sourceKind").map(js_string).unwrap_or_else(|| "undefined".into());
        let locator = source.get("locator").map(js_string).unwrap_or_else(|| "undefined".into());
        if !identities.insert(format!("{kind}\n{locator}")) {
            continue;
        }
        source["ordinal"] = json!(unique.len());
        unique.push(source);
    }
    unique
}

/// 응답 출처 저장 실패. 검증 실패는 완료 처리에서 경고로 바뀌고, DB 실패는 그대로 올린다.
#[derive(Debug)]
pub enum SourceStoreError {
    Invalid(ProvenanceValidationError),
    Database(sqlx::Error),
}

impl From<sqlx::Error> for SourceStoreError {
    fn from(error: sqlx::Error) -> Self {
        Self::Database(error)
    }
}

const INSERT_SOURCE_SQL: &str = r#"
        INSERT INTO turn_response_sources (
          turn_id,
          ordinal,
          source_kind,
          title,
          locator,
          excerpt,
          rag_document_id,
          work_record_id,
          metadata
        )
        VALUES (
          $1::uuid,
          $2,
          $3,
          $4,
          $5,
          $6,
          $7::uuid,
          $8::uuid,
          $9::jsonb
        )
        RETURNING json_build_object(
          'id', id,
          'sourceKind', source_kind,
          'title', title,
          'locator', locator,
          'excerpt', excerpt,
          'ragDocumentId', rag_document_id,
          'workRecordId', work_record_id
        )
"#;

/// replaceTurnResponseSources. 참조한 RAG 문서·작업 기록이 모두 있을 때만 턴의 출처를 통째로 바꾸고 저장한 행을 돌려준다.
pub async fn replace_turn_response_sources(
    connection: &mut PgConnection,
    turn_id: &str,
    sources: &[Value],
) -> Result<Vec<Value>, SourceStoreError> {
    validate_source_reference_set(connection, sources, "ragDocumentID", "rag_documents", "RAG 출처 문서").await?;
    validate_source_reference_set(connection, sources, "workRecordID", "work_records", "DB 작업 기록").await?;
    sqlx::query("DELETE FROM turn_response_sources WHERE turn_id = $1::uuid")
        .bind(turn_id)
        .execute(&mut *connection)
        .await?;
    let mut stored = Vec::with_capacity(sources.len());
    for source in sources {
        let text = |key: &str| source.get(key).and_then(Value::as_str);
        let row: Value = sqlx::query_scalar(INSERT_SOURCE_SQL)
            .bind(turn_id)
            .bind(source.get("ordinal").and_then(Value::as_i64))
            .bind(text("sourceKind"))
            .bind(text("title"))
            .bind(text("locator"))
            .bind(text("excerpt"))
            .bind(text("ragDocumentID"))
            .bind(text("workRecordID"))
            .bind(source.get("metadata").map(Value::to_string))
            .fetch_one(&mut *connection)
            .await?;
        stored.push(row);
    }
    Ok(stored)
}

/// validateSourceReferenceSet. 같은 ID는 한 번만 묻고, 하나라도 없으면 검증 오류다.
async fn validate_source_reference_set(
    connection: &mut PgConnection,
    sources: &[Value],
    key: &str,
    table: &str,
    label: &str,
) -> Result<(), SourceStoreError> {
    let mut ids: Vec<String> = Vec::new();
    for id in sources.iter().filter_map(|source| source.get(key).and_then(Value::as_str)).filter(|id| !id.is_empty()) {
        if !ids.iter().any(|known| known == id) {
            ids.push(id.to_string());
        }
    }
    if ids.is_empty() {
        return Ok(());
    }
    let found: Vec<String> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "SELECT id::text FROM {table} WHERE id = ANY($1::uuid[])"
    )))
    .bind(&ids)
    .fetch_all(&mut *connection)
    .await?;
    if ids.iter().any(|id| !found.contains(id)) {
        return Err(SourceStoreError::Invalid(ProvenanceValidationError(format!("{label} 참조를 찾을 수 없습니다."))));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(query: &str) -> Result<WorkRecordFilters, ProvenanceValidationError> {
        parse_work_record_filters(&SearchParams::parse(Some(query)))
    }

    #[test]
    fn 작업_기록_조회_조건은_기본_범위와_필터를_정규화한다() {
        assert_eq!(
            parse("").unwrap(),
            WorkRecordFilters {
                project_id: None,
                record_type: None,
                lifecycle_state: None,
                attribution: None,
                query: None,
                limit: 50.0,
                offset: 0.0,
            }
        );
        let filtered = parse(
            "projectId=2B6181F5-dbb0-4c19-ae2e-0fcb8a42a349&kind=decision&state=legacy&attribution=unknown&q=+%EC%84%B8%EC%85%98+%EC%9C%A0%EC%A7%80+&limit=25&offset=50",
        )
        .unwrap();
        assert_eq!(filtered.project_id.as_deref(), Some("2b6181f5-dbb0-4c19-ae2e-0fcb8a42a349"));
        assert_eq!(filtered.query.as_deref(), Some("세션 유지"));
        assert_eq!(filtered.record_type.as_deref(), Some("decision"));
        assert_eq!(filtered.limit, 25.0);
        assert_eq!(filtered.offset, 50.0);
    }

    #[test]
    fn 작업_기록_조회_조건은_잘못된_uuid와_범위를_거절한다() {
        assert_eq!(parse("projectId=office").unwrap_err().0, "projectId 값은 UUID여야 합니다.");
        assert_eq!(parse("limit=101").unwrap_err().0, "limit 값은 1 이상 100 이하 정수여야 합니다.");
        assert_eq!(parse("state=deleted").unwrap_err().0, "state 값이 올바르지 않습니다.");
        assert_eq!(parse("offset=1.5").unwrap_err().0, "offset 값은 0 이상 100000 이하 정수여야 합니다.");
        assert_eq!(parse(&format!("q={}", "가".repeat(501))).unwrap_err().0, "q 값은 500자 이하여야 합니다.");
    }

    #[test]
    fn 숫자는_js_number로_읽는다() {
        assert_eq!(parse("limit=0x10&offset=1e2").map(|filters| (filters.limit, filters.offset)), Ok((16.0, 100.0)));
        assert_eq!(parse("limit=+25+").map(|filters| filters.limit), Ok(25.0));
        // 같은 이름은 처음 값을 쓴다.
        assert_eq!(parse("kind=task&kind=wrong").map(|filters| filters.record_type), Ok(Some("task".into())));
    }

    #[test]
    fn 업무_폴더_안_파일_출처는_병합_뒤에도_쓸_상대경로로_바꾼다() {
        let sources = normalize_response_sources(
            &serde_json::json!([
                { "kind": "file", "title": "README", "locator": "/tmp/worktree/project/README.md:8-10" },
                { "kind": "file", "title": "외부 설정", "locator": "/tmp/shared/settings.json" },
                { "kind": "file", "title": "같은 README", "locator": "README.md:8-10" },
                { "kind": "file", "title": "업무 폴더 자체", "locator": "/tmp/worktree/project" },
                { "kind": "file", "title": "형제 폴더", "locator": "/tmp/worktree/project-other/a.rs:3" },
                { "kind": "file", "title": "깊은 파일", "locator": "/tmp/worktree/project/src/./x/../y.rs:12" },
                { "kind": "web", "title": "웹", "locator": "https://example.com/a" },
            ]),
            50,
        )
        .unwrap();
        let locators = |workdir: &str| -> Vec<(u64, String)> {
            portable_response_sources(&sources, workdir)
                .iter()
                .map(|source| (source["ordinal"].as_u64().unwrap(), source["locator"].as_str().unwrap().to_string()))
                .collect()
        };
        let owned = |items: &[(u64, &str)]| items.iter().map(|(ordinal, locator)| (*ordinal, locator.to_string())).collect::<Vec<_>>();
        // Node v24 portableResponseSources 결과.
        let inside = owned(&[
            (0, "README.md:8-10"),
            (1, "/tmp/shared/settings.json"),
            (2, "/tmp/worktree/project"),
            (3, "/tmp/worktree/project-other/a.rs:3"),
            (4, "src/y.rs:12"),
            (5, "https://example.com/a"),
        ]);
        assert_eq!(locators("/tmp/worktree/project"), inside);
        assert_eq!(locators("/tmp/worktree/project/"), inside);
        assert_eq!(
            locators("  "),
            owned(&[
                (0, "/tmp/worktree/project/README.md:8-10"),
                (1, "/tmp/shared/settings.json"),
                (2, "README.md:8-10"),
                (3, "/tmp/worktree/project"),
                (4, "/tmp/worktree/project-other/a.rs:3"),
                (5, "/tmp/worktree/project/src/./x/../y.rs:12"),
                (6, "https://example.com/a"),
            ])
        );
        assert_eq!(
            locators("/tmp/worktree"),
            owned(&[
                (0, "project/README.md:8-10"),
                (1, "/tmp/shared/settings.json"),
                (2, "README.md:8-10"),
                (3, "project"),
                (4, "project-other/a.rs:3"),
                (5, "project/src/y.rs:12"),
                (6, "https://example.com/a"),
            ])
        );
        // 바꾼 출처도 나머지 필드와 키 순서는 그대로다.
        assert_eq!(
            serde_json::to_string(&portable_response_sources(&sources, "/tmp/worktree/project")[4]).unwrap(),
            r#"{"ordinal":4,"sourceKind":"file","title":"깊은 파일","locator":"src/y.rs:12","excerpt":null,"ragDocumentID":null,"workRecordID":null,"metadata":{}}"#
        );
    }
}
