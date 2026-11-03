// 공통 프로젝트·티켓 데이터 계층. Node의 work-board.mjs를 옮겼다. 배정은 실행 요청과 독립적이다.

use std::collections::{HashMap, HashSet};
use std::fmt;

use serde_json::{Map, Value, json};
use sqlx::{Acquire, AssertSqlSafe, PgConnection, Postgres};

type Object = Map<String, Value>;
pub type BoardResult<T> = Result<T, WorkBoardError>;

#[derive(Debug)]
pub enum WorkBoardError {
    /// Node의 WorkBoardError(message, status).
    Rejected { status: u16, message: String },
    /// WorkBoardError가 아닌 DB 오류. Node에서는 바깥 처리기가 500과 pg 오류 문구로 낸다.
    Database(sqlx::Error),
}

impl WorkBoardError {
    fn new(message: impl Into<String>) -> Self {
        Self::with_status(message, 400)
    }

    pub fn with_status(message: impl Into<String>, status: u16) -> Self {
        Self::Rejected { status, message: message.into() }
    }

    pub fn status(&self) -> u16 {
        match self {
            Self::Rejected { status, .. } => *status,
            Self::Database(_) => 500,
        }
    }

    fn database_code(&self) -> Option<String> {
        match self {
            Self::Database(error) => error.as_database_error()?.code().map(|code| code.into_owned()),
            Self::Rejected { .. } => None,
        }
    }
}

impl fmt::Display for WorkBoardError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Rejected { message, .. } => f.write_str(message),
            // Node의 error.message처럼 DB가 준 문구만 쓴다.
            Self::Database(error) => match error.as_database_error() {
                Some(database_error) => f.write_str(database_error.message()),
                None => write!(f, "{error}"),
            },
        }
    }
}

impl From<sqlx::Error> for WorkBoardError {
    fn from(error: sqlx::Error) -> Self {
        Self::Database(error)
    }
}

const STATES: [&str; 6] = ["open", "in_progress", "review", "done", "canceled", "deferred"];
const TICKET_TYPES: [&str; 8] =
    ["general", "planning", "implementation", "pretest", "verification", "research", "content", "operations"];
const USER_REVIEWS: [&str; 3] = ["none", "approved", "changes_requested"];
const VERIFICATION_VERDICTS: [&str; 2] = ["pass", "fail"];
const OPERATION_IMPACTS: [&str; 2] = ["internal", "external"];
const ACTIVITY_KINDS: [&str; 3] = ["progress", "decision", "verification"];
const TICKET_FIELDS: [&str; 23] = [
    "title", "description", "assigneeId", "completedById", "verifiedById", "state", "dueDate",
    "completionCriteria", "parentTicketId", "predecessorIds", "goalIds", "workRecordIds",
    "decisionPending", "reportedActorId", "pushedCommitSha", "ticketType", "completionEvidence",
    "userReview", "userReviewNote", "targetTicketId", "verificationVerdict", "operationImpact",
    "resolutionReason",
];
const GOAL_FIELDS: [&str; 8] =
    ["horizon", "periodStart", "title", "description", "metricName", "metricUnit", "targetValue", "actualValue"];

/// 원격이 없는 별도 저장소라 푸시 SHA가 생기지 않는 프로젝트다.
pub const NO_REMOTE_PROJECT_KEYS: [&str; 2] = ["toss-trading", "one-click-studio"];

/// JS String.prototype.trim이 지우는 공백. Rust trim과 달리 U+FEFF를 지우고 U+0085는 남긴다.
fn is_js_whitespace(c: char) -> bool {
    matches!(
        c,
        '\u{9}'..='\u{d}' | ' ' | '\u{a0}' | '\u{1680}' | '\u{2000}'..='\u{200a}'
            | '\u{2028}' | '\u{2029}' | '\u{202f}' | '\u{205f}' | '\u{3000}' | '\u{feff}'
    )
}

fn js_trim(value: &str) -> &str {
    value.trim_matches(is_js_whitespace)
}

/// JS 문자열의 length(UTF-16 단위 수).
fn js_length(value: &str) -> usize {
    value.encode_utf16().count()
}

/// JS 참 거짓 판정. 이 표의 값은 문자열·불리언·null·배열뿐이다.
fn truthy(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) => false,
        Some(Value::Bool(flag)) => *flag,
        Some(Value::String(text)) => !text.is_empty(),
        Some(Value::Number(number)) => number.as_f64().is_some_and(|n| n != 0.0),
        Some(_) => true,
    }
}

/// JS `value ?? 기본값`에서 기본값으로 가는 경우(없음·null)를 None으로 접는다.
fn nullish(value: Option<&Value>) -> Option<&Value> {
    value.filter(|value| !value.is_null())
}

fn str_of<'a>(map: &'a Object, key: &str) -> Option<&'a str> {
    map.get(key).and_then(Value::as_str)
}

/// `map[key] === null`. 키가 없으면(undefined) null이 아니다.
fn is_null(map: &Object, key: &str) -> bool {
    matches!(map.get(key), Some(Value::Null))
}

fn object(value: &Value) -> BoardResult<&Object> {
    value.as_object().ok_or_else(|| WorkBoardError::new("JSON 객체가 필요합니다."))
}

fn allowed_fields(value: &Object, allowed: &[&str]) -> BoardResult<()> {
    match value.keys().find(|key| !allowed.contains(&key.as_str())) {
        Some(key) => Err(WorkBoardError::new(format!("알 수 없는 필드: {key}"))),
        None => Ok(()),
    }
}

fn text(value: Option<&Value>, field: &str, max: usize, required: bool) -> BoardResult<String> {
    match value {
        Some(Value::String(raw)) if js_length(raw) <= max && !(required && js_trim(raw).is_empty()) => {
            Ok(js_trim(raw).to_string())
        }
        _ => Err(WorkBoardError::new(format!("{field} 값이 올바르지 않습니다."))),
    }
}

/// `text(value ?? "", field, max)`.
fn text_or_empty(value: Option<&Value>, field: &str, max: usize) -> BoardResult<String> {
    match nullish(value) {
        None => Ok(String::new()),
        some => text(some, field, max, false),
    }
}

/// `value === null || value === undefined ? null : text(value, field, max, true)`.
fn optional_text(value: Option<&Value>, field: &str, max: usize) -> BoardResult<Value> {
    match nullish(value) {
        None => Ok(Value::Null),
        some => Ok(Value::String(text(some, field, max, true)?)),
    }
}

fn is_uuid(value: &str) -> bool {
    value.len() == 36
        && value.bytes().enumerate().all(|(index, byte)| {
            if [8, 13, 18, 23].contains(&index) { byte == b'-' } else { byte.is_ascii_hexdigit() }
        })
}

fn uuid(value: Option<&Value>, field: &str) -> BoardResult<String> {
    match value {
        Some(Value::String(id)) if is_uuid(id) => Ok(id.clone()),
        _ => Err(WorkBoardError::new(format!("{field}는 UUID여야 합니다."))),
    }
}

/// 경로에서 받은 id 검사. Node의 `uuid(id, field)`.
fn require_uuid(id: &str, field: &str) -> BoardResult<()> {
    if is_uuid(id) { Ok(()) } else { Err(WorkBoardError::new(format!("{field}는 UUID여야 합니다."))) }
}

/// `value === null || value === undefined ? null : uuid(value, field)`.
fn optional_uuid(value: Option<&Value>, field: &str) -> BoardResult<Value> {
    match nullish(value) {
        None => Ok(Value::Null),
        some => Ok(Value::String(uuid(some, field)?)),
    }
}

fn is_date_shape(text: &str) -> bool {
    let bytes = text.as_bytes();
    bytes.len() == 10
        && bytes
            .iter()
            .enumerate()
            .all(|(index, byte)| if index == 4 || index == 7 { *byte == b'-' } else { byte.is_ascii_digit() })
}

fn date_parts(text: &str) -> (i64, u32, u32) {
    let number = |range: std::ops::Range<usize>| text[range].parse().unwrap_or(0);
    (number(0..4) as i64, number(5..7), number(8..10))
}

fn days_in_month(year: i64, month: u32) -> u32 {
    let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    match month {
        2 if leap => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

/// 1970-01-01부터의 날짜 수(그레고리력 연장). Howard Hinnant의 days_from_civil.
fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year - era * 400;
    let month_from_march = (i64::from(month) + 9) % 12;
    let day_of_year = (153 * month_from_march + 2) / 5 + i64::from(day) - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

/// JS `new Date(`${text}T00:00:00Z`).getUTCDay()`. 0이 일요일이다.
fn utc_weekday(text: &str) -> i64 {
    let (year, month, day) = date_parts(text);
    (days_from_civil(year, month, day) + 4).rem_euclid(7)
}

/// Node의 dueDate(). 없음·null은 null, 형식이 맞아도 달력에 없는 날은 거부한다. 기간 시작(periodStart)도 이 문구를 쓴다.
fn due_date(value: Option<&Value>) -> BoardResult<Value> {
    let Some(value) = nullish(value) else {
        return Ok(Value::Null);
    };
    let Some(text) = value.as_str().filter(|text| is_date_shape(text)) else {
        return Err(WorkBoardError::new("dueDate는 YYYY-MM-DD 또는 null이어야 합니다."));
    };
    let (year, month, day) = date_parts(text);
    if !(1..=12).contains(&month) || day == 0 || day > days_in_month(year, month) {
        return Err(WorkBoardError::new("dueDate가 유효한 날짜가 아닙니다."));
    }
    Ok(Value::String(text.to_string()))
}

fn uuid_list(value: Option<&Value>, field: &str) -> BoardResult<Vec<String>> {
    let Some(Value::Array(items)) = value.filter(|value| value.as_array().is_some_and(|items| items.len() <= 100))
    else {
        return Err(WorkBoardError::new(format!("{field}는 UUID 목록이어야 합니다.")));
    };
    let ids = items.iter().map(|item| uuid(Some(item), field)).collect::<BoardResult<Vec<_>>>()?;
    if ids.iter().collect::<HashSet<_>>().len() != ids.len() {
        return Err(WorkBoardError::new(format!("{field}에 중복이 있습니다.")));
    }
    Ok(ids)
}

/// Node의 decimalText(). 정수 16자리·소수 4자리까지의 문자열만 받는다.
fn decimal_text(value: Option<&Value>, field: &str, positive: bool) -> BoardResult<Value> {
    let Some(value) = nullish(value) else {
        return Ok(Value::Null);
    };
    let digits = |part: &str, max: usize| (1..=max).contains(&part.len()) && part.bytes().all(|b| b.is_ascii_digit());
    let zeros = |part: &str| part.bytes().all(|b| b == b'0');
    let parsed = value.as_str().and_then(|text| {
        let (whole, fraction) = match text.split_once('.') {
            Some((whole, fraction)) => (whole, Some(fraction)),
            None => (text, None),
        };
        (digits(whole, 16) && fraction.is_none_or(|fraction| digits(fraction, 4))).then_some((text, whole, fraction))
    });
    let Some((text, whole, fraction)) = parsed else {
        return Err(WorkBoardError::new(format!("{field}는 십진수 문자열 또는 null이어야 합니다.")));
    };
    if positive && zeros(whole) && fraction.is_none_or(zeros) {
        return Err(WorkBoardError::new(format!("{field}는 0보다 커야 합니다.")));
    }
    Ok(Value::String(text.to_string()))
}

/// `value ?? (생성 ? 기본값 : null)`을 허용 목록으로 검사한다.
fn enumerated(value: Option<&Value>, default: Option<&'static str>, allowed: &[&str], message: &str) -> BoardResult<Value> {
    let chosen = match nullish(value) {
        Some(value) => value.as_str(),
        None => default,
    };
    match chosen.filter(|chosen| allowed.contains(chosen)) {
        Some(chosen) => Ok(Value::String(chosen.to_string())),
        None => Err(WorkBoardError::new(message)),
    }
}

/// `value ?? null`이 null이 아니면 허용 목록 검사.
fn optional_enumerated(value: Option<&Value>, allowed: &[&str], message: &str) -> BoardResult<Value> {
    match nullish(value) {
        None => Ok(Value::Null),
        some => enumerated(some, None, allowed, message),
    }
}

pub fn normalize_project_input(body: &Value, patch: bool) -> BoardResult<Object> {
    let value = object(body)?;
    allowed_fields(value, if patch { &["title", "description"] } else { &["projectKey", "title", "description"] })?;
    if patch && value.is_empty() {
        return Err(WorkBoardError::new("수정할 필드가 없습니다."));
    }
    let has = |key: &str| !patch || value.contains_key(key);
    let mut result = Object::new();
    if !patch {
        let key = value.get("projectKey").and_then(Value::as_str).filter(|key| {
            (2..=64).contains(&key.len()) && key.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        });
        let Some(key) = key else {
            return Err(WorkBoardError::new("projectKey는 소문자·숫자·하이픈 2~64자여야 합니다."));
        };
        result.insert("projectKey".into(), key.into());
    }
    if has("title") {
        result.insert("title".into(), text(value.get("title"), "title", 120, true)?.into());
    }
    if has("description") {
        result.insert("description".into(), text_or_empty(value.get("description"), "description", 4000)?.into());
    }
    Ok(result)
}

pub fn normalize_ticket_input(body: &Value, patch: bool) -> BoardResult<Object> {
    let value = object(body)?;
    if patch {
        allowed_fields(value, &TICKET_FIELDS)?;
    } else {
        allowed_fields(value, &[&["projectId"][..], &TICKET_FIELDS[..]].concat())?;
    }
    if patch && value.is_empty() {
        return Err(WorkBoardError::new("수정할 필드가 없습니다."));
    }
    let has = |key: &str| !patch || value.contains_key(key);
    let create_default = |default: &'static str| (!patch).then_some(default);
    let mut result = Object::new();
    if !patch {
        result.insert("projectId".into(), uuid(value.get("projectId"), "projectId")?.into());
    }
    if has("title") {
        result.insert("title".into(), text(value.get("title"), "title", 160, true)?.into());
    }
    if has("description") {
        result.insert("description".into(), text_or_empty(value.get("description"), "description", 10000)?.into());
    }
    if has("completionCriteria") {
        let criteria = text_or_empty(value.get("completionCriteria"), "completionCriteria", 4000)?;
        result.insert("completionCriteria".into(), criteria.into());
    }
    if has("ticketType") {
        let ticket_type =
            enumerated(value.get("ticketType"), create_default("general"), &TICKET_TYPES, "ticketType이 올바르지 않습니다.")?;
        result.insert("ticketType".into(), ticket_type);
    }
    for field in ["completionEvidence", "userReviewNote", "resolutionReason"] {
        if has(field) {
            let max = if field == "completionEvidence" { 10000 } else { 4000 };
            result.insert(field.into(), text_or_empty(value.get(field), field, max)?.into());
        }
    }
    if has("userReview") {
        let review =
            enumerated(value.get("userReview"), create_default("none"), &USER_REVIEWS, "userReview가 올바르지 않습니다.")?;
        result.insert("userReview".into(), review);
    }
    if has("targetTicketId") {
        result.insert("targetTicketId".into(), optional_uuid(value.get("targetTicketId"), "targetTicketId")?);
    }
    if has("verificationVerdict") {
        let verdict = optional_enumerated(
            value.get("verificationVerdict"),
            &VERIFICATION_VERDICTS,
            "verificationVerdict가 올바르지 않습니다.",
        )?;
        result.insert("verificationVerdict".into(), verdict);
    }
    if has("operationImpact") {
        let impact =
            optional_enumerated(value.get("operationImpact"), &OPERATION_IMPACTS, "operationImpact가 올바르지 않습니다.")?;
        result.insert("operationImpact".into(), impact);
    }
    for field in ["assigneeId", "completedById", "verifiedById"] {
        if has(field) {
            result.insert(field.into(), optional_text(value.get(field), field, 100)?);
        }
    }
    if has("state") {
        let state = if !patch && !value.contains_key("state") { Some("open") } else { value.get("state").and_then(Value::as_str) };
        match state.filter(|state| STATES.contains(state)) {
            Some(state) => result.insert("state".into(), state.into()),
            None => return Err(WorkBoardError::new("state가 올바르지 않습니다.")),
        };
    }
    if has("dueDate") {
        result.insert("dueDate".into(), due_date(value.get("dueDate"))?);
    }
    if has("pushedCommitSha") {
        let sha = match nullish(value.get("pushedCommitSha")) {
            None => Value::Null,
            Some(Value::String(sha)) if sha.len() == 40 && sha.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)) => {
                sha.as_str().into()
            }
            Some(_) => {
                return Err(WorkBoardError::new("pushedCommitSha는 원격에 푸시된 40자리 소문자 커밋 SHA여야 합니다."));
            }
        };
        result.insert("pushedCommitSha".into(), sha);
    }
    if has("decisionPending") {
        let pending = match nullish(value.get("decisionPending")) {
            Some(value) => value.as_bool(),
            None => (!patch).then_some(false),
        };
        let Some(pending) = pending else {
            return Err(WorkBoardError::new("decisionPending은 불리언이어야 합니다."));
        };
        result.insert("decisionPending".into(), pending.into());
    }
    if value.contains_key("reportedActorId") {
        let actor = match value.get("reportedActorId") {
            Some(Value::Null) => Value::Null,
            actor => text(actor, "reportedActorId", 100, true)?.into(),
        };
        result.insert("reportedActorId".into(), actor);
    }
    if has("parentTicketId") {
        result.insert("parentTicketId".into(), optional_uuid(value.get("parentTicketId"), "parentTicketId")?);
    }
    let empty = Value::Array(Vec::new());
    for field in ["predecessorIds", "goalIds", "workRecordIds"] {
        if has(field) {
            let list = if !patch && !value.contains_key(field) { Some(&empty) } else { value.get(field) };
            result.insert(field.into(), uuid_list(list, field)?.into());
        }
    }
    Ok(result)
}

pub fn normalize_activity_input(body: &Value) -> BoardResult<Object> {
    let value = object(body)?;
    allowed_fields(value, &["kind", "body", "reportedActorId"])?;
    let Some(kind) = value.get("kind").and_then(Value::as_str).filter(|kind| ACTIVITY_KINDS.contains(kind)) else {
        return Err(WorkBoardError::new("kind가 올바르지 않습니다."));
    };
    let mut result = Object::new();
    result.insert("kind".into(), kind.into());
    result.insert("body".into(), text(value.get("body"), "body", 4000, true)?.into());
    result.insert("reportedActorId".into(), optional_text(value.get("reportedActorId"), "reportedActorId", 100)?);
    Ok(result)
}

pub fn normalize_goal_input(body: &Value, patch: bool) -> BoardResult<Object> {
    let value = object(body)?;
    if patch {
        allowed_fields(value, &GOAL_FIELDS)?;
    } else {
        allowed_fields(value, &[&["projectId"][..], &GOAL_FIELDS[..]].concat())?;
    }
    if patch && value.is_empty() {
        return Err(WorkBoardError::new("수정할 필드가 없습니다."));
    }
    let has = |key: &str| !patch || value.contains_key(key);
    let mut result = Object::new();
    if !patch {
        result.insert("projectId".into(), uuid(value.get("projectId"), "projectId")?.into());
    }
    if has("horizon") {
        let Some(horizon) = value.get("horizon").and_then(Value::as_str).filter(|h| ["weekly", "monthly"].contains(h))
        else {
            return Err(WorkBoardError::new("horizon은 weekly 또는 monthly여야 합니다."));
        };
        result.insert("horizon".into(), horizon.into());
    }
    if has("periodStart") {
        if nullish(value.get("periodStart")).is_none() {
            return Err(WorkBoardError::new("periodStart가 필요합니다."));
        }
        result.insert("periodStart".into(), due_date(value.get("periodStart"))?);
    }
    if has("title") {
        result.insert("title".into(), text(value.get("title"), "title", 160, true)?.into());
    }
    if has("description") {
        result.insert("description".into(), text_or_empty(value.get("description"), "description", 10000)?.into());
    }
    for (key, max) in [("metricName", 120), ("metricUnit", 40)] {
        if has(key) {
            result.insert(key.into(), optional_text(value.get(key), key, max)?);
        }
    }
    if has("targetValue") {
        result.insert("targetValue".into(), decimal_text(value.get("targetValue"), "targetValue", true)?);
    }
    if has("actualValue") {
        result.insert("actualValue".into(), decimal_text(value.get("actualValue"), "actualValue", false)?);
    }
    if !patch {
        validate_goal_period(str_of(&result, "horizon").unwrap_or(""), str_of(&result, "periodStart").unwrap_or(""))?;
        if (!is_null(&result, "targetValue") || !is_null(&result, "actualValue")) && !truthy(result.get("metricName")) {
            return Err(WorkBoardError::new("수치 목표에는 metricName이 필요합니다."));
        }
    }
    Ok(result)
}

fn validate_goal_period(horizon: &str, period_start: &str) -> BoardResult<()> {
    if (horizon == "weekly" && utc_weekday(period_start) != 1) || (horizon == "monthly" && !period_start.ends_with("-01")) {
        return Err(WorkBoardError::new("주간 시작은 월요일, 월간 시작은 매월 1일이어야 합니다."));
    }
    Ok(())
}

/// timestamptz를 JS Date의 JSON 표기(UTC, 밀리초, Z)로 바꾼다. pg 드라이버처럼 밀리초 아래는 버린다.
macro_rules! iso {
    ($column:literal) => {
        concat!("to_char(", $column, " AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z\"')")
    };
}

macro_rules! project_columns {
    () => {
        concat!(
            r#"id, project_key AS "projectKey", title, description, "#,
            iso!("created_at"), r#" AS "createdAt", "#, iso!("updated_at"), r#" AS "updatedAt""#
        )
    };
}

macro_rules! ticket_columns {
    () => {
        concat!(
            r#"id, project_id AS "projectId", title, description,
  assignee_id AS "assigneeId", completed_by_id AS "completedById",
  verified_by_id AS "verifiedById", state, pushed_commit_sha AS "pushedCommitSha",
  ticket_type AS "ticketType", completion_evidence AS "completionEvidence",
  user_review AS "userReview", user_review_note AS "userReviewNote",
  "#,
            iso!("user_reviewed_at"),
            r#" AS "userReviewedAt", target_ticket_id AS "targetTicketId",
  verification_verdict AS "verificationVerdict", operation_impact AS "operationImpact",
  resolution_reason AS "resolutionReason",
  due_date::text AS "dueDate",
  completion_criteria AS "completionCriteria", decision_pending AS "decisionPending",
  parent_ticket_id AS "parentTicketId",
  "#,
            iso!("created_at"), r#" AS "createdAt", "#, iso!("updated_at"), r#" AS "updatedAt""#
        )
    };
}

macro_rules! goal_columns {
    () => {
        concat!(
            r#"id, project_id AS "projectId", horizon,
  period_start::text AS "periodStart", title, description,
  metric_name AS "metricName", metric_unit AS "metricUnit",
  target_value::text AS "targetValue", actual_value::text AS "actualValue",
  "#,
            iso!("created_at"), r#" AS "createdAt", "#, iso!("updated_at"), r#" AS "updatedAt""#
        )
    };
}

macro_rules! activity_columns {
    () => {
        concat!(
            r#"id, ticket_id AS "ticketId", kind, body, changes,
  reported_actor_id AS "reportedActorId", "#,
            iso!("created_at"), r#" AS "createdAt""#
        )
    };
}

// 행은 row_to_json으로 받는다. json(jsonb 아님)은 열 순서를 지키므로 Node 응답과 키 순서가 같다.
// 목록은 LATERAL로 감싸 바깥 ORDER BY가 순서를 보장하게 한다.

fn grouped(pairs: impl IntoIterator<Item = (String, Value)>) -> HashMap<String, Vec<Value>> {
    let mut groups: HashMap<String, Vec<Value>> = HashMap::new();
    for (key, value) in pairs {
        groups.entry(key).or_default().push(value);
    }
    groups
}

pub async fn list_board<'c>(db: impl Acquire<'c, Database = Postgres>) -> BoardResult<Value> {
    let mut connection = db.acquire().await?;
    let connection: &mut PgConnection = &mut connection;
    let projects: Vec<Value> = sqlx::query_scalar(concat!(
        "SELECT row_to_json(x) FROM work_board_projects AS source CROSS JOIN LATERAL (SELECT ",
        project_columns!(),
        ") AS x ORDER BY source.created_at, source.project_key"
    ))
    .fetch_all(&mut *connection)
    .await?;
    let tickets: Vec<Value> = sqlx::query_scalar(concat!(
        "SELECT row_to_json(x) FROM work_board_tickets AS source CROSS JOIN LATERAL (SELECT ",
        ticket_columns!(),
        ") AS x ORDER BY source.created_at, source.id"
    ))
    .fetch_all(&mut *connection)
    .await?;
    let goals: Vec<Value> = sqlx::query_scalar(concat!(
        "SELECT row_to_json(x) FROM work_board_goals AS source CROSS JOIN LATERAL (SELECT ",
        goal_columns!(),
        ") AS x ORDER BY source.period_start DESC, source.created_at, source.id"
    ))
    .fetch_all(&mut *connection)
    .await?;
    let dependencies: Vec<(String, String)> = sqlx::query_as(
        "SELECT ticket_id::text, predecessor_id::text
         FROM work_board_ticket_dependencies ORDER BY ticket_id, predecessor_id",
    )
    .fetch_all(&mut *connection)
    .await?;
    let goal_links: Vec<(String, String)> =
        sqlx::query_as("SELECT ticket_id::text, goal_id::text FROM work_board_goal_tickets ORDER BY ticket_id, goal_id")
            .fetch_all(&mut *connection)
            .await?;
    let work_records: Vec<Value> = sqlx::query_scalar(concat!(
        r#"SELECT row_to_json(x) FROM work_board_ticket_records AS link
     JOIN work_records AS record ON record.id = link.work_record_id
     CROSS JOIN LATERAL (SELECT link.ticket_id AS "ticketId", record.id,
       record.title, record.record_type AS "recordType",
       record.character_id AS "characterId", record.source_turn_id AS "sourceTurnId",
       "#,
        iso!("record.recorded_at"),
        r#" AS "recordedAt",
       left(CASE WHEN strpos(record.body, E'결과\n') > 0
         THEN substr(record.body, strpos(record.body, E'결과\n') + char_length(E'결과\n'))
         ELSE record.body END, 220) AS "bodyPreview") AS x
     ORDER BY record.recorded_at DESC, record.id"#
    ))
    .fetch_all(&mut *connection)
    .await?;
    let mut predecessor_ids = grouped(dependencies.into_iter().map(|(ticket, id)| (ticket, Value::String(id))));
    let mut goal_ids = grouped(goal_links.into_iter().map(|(ticket, id)| (ticket, Value::String(id))));
    let mut work_record_ids = grouped(
        work_records
            .iter()
            .map(|record| (record["ticketId"].as_str().unwrap_or_default().to_string(), record["id"].clone())),
    );
    let tickets: Vec<Value> = tickets
        .into_iter()
        .map(|mut ticket| {
            let id = ticket["id"].as_str().unwrap_or_default().to_string();
            if let Some(row) = ticket.as_object_mut() {
                row.insert("predecessorIds".into(), predecessor_ids.remove(&id).unwrap_or_default().into());
                row.insert("goalIds".into(), goal_ids.remove(&id).unwrap_or_default().into());
                row.insert("workRecordIds".into(), work_record_ids.remove(&id).unwrap_or_default().into());
            }
            ticket
        })
        .collect();
    Ok(json!({ "projects": projects, "goals": goals, "workRecords": work_records, "tickets": tickets }))
}

async fn read_ticket(connection: &mut PgConnection, id: &str) -> BoardResult<Object> {
    let row: Option<Value> = sqlx::query_scalar(concat!(
        "SELECT row_to_json(x) FROM (SELECT ",
        ticket_columns!(),
        " FROM work_board_tickets WHERE id = $1::uuid) AS x"
    ))
    .bind(id)
    .fetch_optional(&mut *connection)
    .await?;
    let Some(Value::Object(mut ticket)) = row else {
        return Err(WorkBoardError::with_status("티켓이 없습니다.", 404));
    };
    // Node는 `ORDER BY id`(출력 별칭 = 원래 uuid 열)로 정렬한다. 텍스트 정렬이 되지 않게 원래 열로 정렬한다.
    let predecessors: Vec<String> = sqlx::query_scalar(
        "SELECT predecessor_id::text FROM work_board_ticket_dependencies WHERE ticket_id = $1::uuid ORDER BY predecessor_id",
    )
    .bind(id)
    .fetch_all(&mut *connection)
    .await?;
    let goals: Vec<String> =
        sqlx::query_scalar("SELECT goal_id::text FROM work_board_goal_tickets WHERE ticket_id = $1::uuid ORDER BY goal_id")
            .bind(id)
            .fetch_all(&mut *connection)
            .await?;
    let records: Vec<String> = sqlx::query_scalar(
        "SELECT work_record_id::text FROM work_board_ticket_records WHERE ticket_id = $1::uuid ORDER BY work_record_id",
    )
    .bind(id)
    .fetch_all(&mut *connection)
    .await?;
    ticket.insert("predecessorIds".into(), predecessors.into());
    ticket.insert("goalIds".into(), goals.into());
    ticket.insert("workRecordIds".into(), records.into());
    Ok(ticket)
}

pub async fn read_board_work_record<'c>(db: impl Acquire<'c, Database = Postgres>, id: &str) -> BoardResult<Value> {
    require_uuid(id, "workRecordId")?;
    let mut connection = db.acquire().await?;
    let row: Option<Value> = sqlx::query_scalar(concat!(
        r#"SELECT row_to_json(x) FROM (SELECT id, title, body, record_type AS "recordType",
       character_id AS "characterId", source_turn_id AS "sourceTurnId",
       "#,
        iso!("recorded_at"),
        r#" AS "recordedAt"
     FROM work_records WHERE id = $1::uuid) AS x"#
    ))
    .bind(id)
    .fetch_optional(&mut *connection)
    .await?;
    row.ok_or_else(|| WorkBoardError::with_status("업무 기록이 없습니다.", 404))
}

fn change_label(field: &str) -> &str {
    match field {
        "title" => "제목",
        "description" => "설명",
        "assigneeId" => "담당자",
        "completedById" => "완료자",
        "verifiedById" => "검증자",
        "state" => "상태",
        "ticketType" => "유형",
        "completionEvidence" => "완료 근거",
        "userReview" => "사용자 검토",
        "userReviewNote" => "사용자 검토 메모",
        "userReviewedAt" => "사용자 검토 시각",
        "targetTicketId" => "검증 대상",
        "verificationVerdict" => "검증 판정",
        "operationImpact" => "운영 영향",
        "resolutionReason" => "대기·취소 사유",
        "dueDate" => "기한",
        "completionCriteria" => "완료 조건",
        "decisionPending" => "사용자 결정 대기",
        "parentTicketId" => "상위 작업",
        "predecessorIds" => "선행 작업",
        "goalIds" => "목표 연결",
        "workRecordIds" => "업무 기록 연결",
        "pushedCommitSha" => "푸시된 커밋 SHA",
        other => other,
    }
}

/// Node의 display(): JSON 표기가 180자(UTF-16 단위)를 넘으면 잘라 말줄임표를 붙인다.
/// 경계가 서로게이트 쌍 가운데면 JS는 반쪽을 남기지만 여기서는 그 글자를 통째로 뺀다.
fn display(value: Option<&Value>) -> String {
    let serialized = serde_json::to_string(value.unwrap_or(&Value::Null)).unwrap_or_default();
    if js_length(&serialized) <= 180 {
        return serialized;
    }
    let mut units = 0;
    let mut end = 0;
    for (index, c) in serialized.char_indices() {
        units += c.len_utf16();
        if units > 180 {
            break;
        }
        end = index + c.len_utf8();
    }
    format!("{}…", &serialized[..end])
}

fn activity_summary(row: &Object) -> Value {
    match str_of(row, "kind") {
        Some("changed") => {
            let parts: Vec<String> = row
                .get("changes")
                .and_then(Value::as_object)
                .map(|changes| {
                    changes
                        .iter()
                        .map(|(field, change)| {
                            format!("{}: {} → {}", change_label(field), display(change.get("before")), display(change.get("after")))
                        })
                        .collect()
                })
                .unwrap_or_default();
            parts.join(" · ").into()
        }
        Some("created") => "티켓 생성".into(),
        _ => row.get("body").cloned().unwrap_or(Value::Null),
    }
}

pub async fn list_ticket_activity<'c>(db: impl Acquire<'c, Database = Postgres>, id: &str) -> BoardResult<Vec<Value>> {
    require_uuid(id, "ticketId")?;
    let mut connection = db.acquire().await?;
    let exists: Option<i32> = sqlx::query_scalar("SELECT 1 FROM work_board_tickets WHERE id = $1::uuid")
        .bind(id)
        .fetch_optional(&mut *connection)
        .await?;
    if exists.is_none() {
        return Err(WorkBoardError::with_status("티켓이 없습니다.", 404));
    }
    let rows: Vec<Value> = sqlx::query_scalar(concat!(
        "SELECT row_to_json(x) FROM work_board_ticket_activity AS source CROSS JOIN LATERAL (SELECT ",
        activity_columns!(),
        ") AS x WHERE source.ticket_id = $1::uuid ORDER BY source.created_at DESC, source.id DESC LIMIT 100"
    ))
    .bind(id)
    .fetch_all(&mut *connection)
    .await?;
    Ok(rows
        .into_iter()
        .map(|mut row| {
            if let Some(object) = row.as_object_mut() {
                let summary = activity_summary(object);
                object.insert("summary".into(), summary);
            }
            row
        })
        .collect())
}

pub async fn add_ticket_activity(connection: &mut PgConnection, id: &str, body: &Value) -> BoardResult<Value> {
    require_uuid(id, "ticketId")?;
    let value = normalize_activity_input(body)?;
    let row: Option<Value> = sqlx::query_scalar(concat!(
        "WITH inserted AS (INSERT INTO work_board_ticket_activity (ticket_id, kind, body, reported_actor_id)
     SELECT id, $2, $3, $4 FROM work_board_tickets WHERE id = $1::uuid
     RETURNING ",
        activity_columns!(),
        ") SELECT row_to_json(inserted) FROM inserted"
    ))
    .bind(id)
    .bind(str_of(&value, "kind"))
    .bind(str_of(&value, "body"))
    .bind(str_of(&value, "reportedActorId"))
    .fetch_optional(&mut *connection)
    .await?;
    let Some(Value::Object(mut activity)) = row else {
        return Err(WorkBoardError::with_status("티켓이 없습니다.", 404));
    };
    let summary = activity.get("body").cloned().unwrap_or(Value::Null);
    activity.insert("summary".into(), summary);
    Ok(Value::Object(activity))
}

async fn write_change_activity(
    connection: &mut PgConnection,
    id: &str,
    kind: &str,
    changes: &Value,
    reported_actor_id: Option<&str>,
) -> BoardResult<()> {
    sqlx::query(
        "INSERT INTO work_board_ticket_activity
       (ticket_id, kind, changes, reported_actor_id) VALUES ($1::uuid, $2, $3::jsonb, $4)",
    )
    .bind(id)
    .bind(kind)
    .bind(changes.to_string())
    .bind(reported_actor_id)
    .execute(&mut *connection)
    .await?;
    Ok(())
}

/// pg 드라이버가 값을 문자열로 보내듯 매개변수를 텍스트로 넘긴다. 열 형식은 SQL 쪽 형 변환이 맞춘다.
fn bind_text(value: &Value) -> Option<String> {
    match value {
        Value::Null => None,
        Value::String(text) => Some(text.clone()),
        other => Some(other.to_string()),
    }
}

/// 입력 키 순서대로 `열 = $n형변환`과 매개변수를 만든다. $1은 id다.
fn assignments(value: &Object, column: fn(&str) -> Option<(&'static str, &'static str)>) -> (Vec<String>, Vec<Option<String>>) {
    let mut sets = Vec::new();
    let mut binds = Vec::new();
    for (key, field) in value {
        let Some((name, cast)) = column(key) else {
            continue;
        };
        binds.push(bind_text(field));
        sets.push(format!("{name} = ${}{cast}", binds.len() + 1));
    }
    (sets, binds)
}

fn project_column(key: &str) -> Option<(&'static str, &'static str)> {
    match key {
        "title" => Some(("title", "")),
        "description" => Some(("description", "")),
        _ => None,
    }
}

fn ticket_column(key: &str) -> Option<(&'static str, &'static str)> {
    Some(match key {
        "title" => ("title", ""),
        "description" => ("description", ""),
        "assigneeId" => ("assignee_id", ""),
        "completedById" => ("completed_by_id", ""),
        "verifiedById" => ("verified_by_id", ""),
        "state" => ("state", ""),
        "dueDate" => ("due_date", "::date"),
        "completionCriteria" => ("completion_criteria", ""),
        "parentTicketId" => ("parent_ticket_id", "::uuid"),
        "decisionPending" => ("decision_pending", "::boolean"),
        "pushedCommitSha" => ("pushed_commit_sha", ""),
        "ticketType" => ("ticket_type", ""),
        "completionEvidence" => ("completion_evidence", ""),
        "userReview" => ("user_review", ""),
        "userReviewNote" => ("user_review_note", ""),
        "targetTicketId" => ("target_ticket_id", "::uuid"),
        "verificationVerdict" => ("verification_verdict", ""),
        "operationImpact" => ("operation_impact", ""),
        "resolutionReason" => ("resolution_reason", ""),
        _ => return None,
    })
}

fn goal_column(key: &str) -> Option<(&'static str, &'static str)> {
    Some(match key {
        "horizon" => ("horizon", ""),
        "periodStart" => ("period_start", "::date"),
        "title" => ("title", ""),
        "description" => ("description", ""),
        "metricName" => ("metric_name", ""),
        "metricUnit" => ("metric_unit", ""),
        "targetValue" => ("target_value", "::numeric"),
        "actualValue" => ("actual_value", "::numeric"),
        _ => return None,
    })
}

fn is_database_code(error: &WorkBoardError, code: &str) -> bool {
    error.database_code().as_deref() == Some(code)
}

pub async fn create_project<'c>(db: impl Acquire<'c, Database = Postgres>, body: &Value) -> BoardResult<Value> {
    let value = normalize_project_input(body, false)?;
    let mut connection = db.acquire().await?;
    let created = sqlx::query_scalar::<_, Value>(concat!(
        "WITH created AS (INSERT INTO work_board_projects (project_key, title, description)
     VALUES ($1, $2, $3) RETURNING ",
        project_columns!(),
        ") SELECT row_to_json(created) FROM created"
    ))
    .bind(str_of(&value, "projectKey"))
    .bind(str_of(&value, "title"))
    .bind(str_of(&value, "description"))
    .fetch_one(&mut *connection)
    .await;
    created.map_err(|error| {
        let error = WorkBoardError::from(error);
        if is_database_code(&error, "23505") { WorkBoardError::with_status("이미 있는 프로젝트 키입니다.", 409) } else { error }
    })
}

pub async fn update_project<'c>(db: impl Acquire<'c, Database = Postgres>, id: &str, body: &Value) -> BoardResult<Value> {
    require_uuid(id, "projectId")?;
    let value = normalize_project_input(body, true)?;
    let (sets, binds) = assignments(&value, project_column);
    let sql = format!(
        "WITH updated AS (UPDATE work_board_projects SET {}, updated_at = now()
     WHERE id = $1::uuid RETURNING {}) SELECT row_to_json(updated) FROM updated",
        sets.join(", "),
        project_columns!()
    );
    let mut connection = db.acquire().await?;
    let mut query = sqlx::query_scalar::<_, Value>(AssertSqlSafe(sql)).bind(id);
    for bind in binds {
        query = query.bind(bind);
    }
    query.fetch_optional(&mut *connection).await?.ok_or_else(|| WorkBoardError::with_status("프로젝트가 없습니다.", 404))
}

struct LockedProject {
    id: String,
    project_key: String,
}

async fn lock_project(connection: &mut PgConnection, project_id: &str) -> BoardResult<LockedProject> {
    let row: Option<(String, String)> =
        sqlx::query_as("SELECT id::text, project_key FROM work_board_projects WHERE id = $1::uuid FOR UPDATE")
            .bind(project_id)
            .fetch_optional(&mut *connection)
            .await?;
    let Some((id, project_key)) = row else {
        return Err(WorkBoardError::with_status("프로젝트가 없습니다.", 404));
    };
    Ok(LockedProject { id, project_key })
}

fn typed_transition_allowed(from: &str, to: &str) -> bool {
    let allowed: &[&str] = match from {
        "open" => &["in_progress", "deferred", "canceled"],
        "in_progress" => &["review", "deferred", "canceled"],
        "review" => &["in_progress", "done", "deferred", "canceled"],
        "deferred" => &["open", "canceled"],
        _ => &[],
    };
    allowed.contains(&to)
}

fn require_delivery_evidence(project: &LockedProject, ticket: &Object) -> BoardResult<()> {
    if !NO_REMOTE_PROJECT_KEYS.contains(&project.project_key.as_str())
        && str_of(ticket, "state") == Some("done")
        && matches!(str_of(ticket, "ticketType"), Some("general" | "implementation"))
        && !truthy(ticket.get("pushedCommitSha"))
    {
        return Err(WorkBoardError::new("구현·미분류 티켓 완료에는 원격 푸시를 확인한 커밋 SHA가 필요합니다."));
    }
    Ok(())
}

struct VerificationTarget {
    id: String,
    assignee_id: Option<String>,
    completed_by_id: Option<String>,
    state: String,
}

/// Node의 validateLifecycle. 검사 순서가 오류 문구를 정하므로 원본 순서를 그대로 따른다.
async fn validate_lifecycle(
    connection: &mut PgConnection,
    project: &LockedProject,
    before: Option<&Object>,
    next: &Object,
    ticket_id: Option<&str>,
) -> BoardResult<Option<VerificationTarget>> {
    let ticket_type = str_of(next, "ticketType").unwrap_or_default();
    let state = str_of(next, "state").unwrap_or_default();
    let typed = ticket_type != "general";
    let reviewing = matches!(state, "review" | "done");
    if let Some(before) = before
        && str_of(before, "state") == Some("done")
        && str_of(before, "ticketType") != Some("general")
        && next.iter().any(|(field, value)| field != "reportedActorId" && before.get(field) != Some(value))
    {
        return Err(WorkBoardError::new("완료된 유형 티켓은 변경할 수 없습니다. 후속 티켓을 만드세요."));
    }
    if before.is_none() && typed && state != "open" {
        return Err(WorkBoardError::new("유형을 지정한 새 티켓은 오픈 상태로 만듭니다."));
    }
    if let Some(before) = before {
        let before_state = str_of(before, "state").unwrap_or_default();
        let before_type = str_of(before, "ticketType").unwrap_or_default();
        if ticket_type != before_type && !matches!(before_state, "open" | "in_progress") {
            return Err(WorkBoardError::new(if matches!(before_state, "done" | "canceled") {
                "완료·취소된 티켓은 유형을 바꿀 수 없습니다. 후속 티켓을 만드세요."
            } else {
                "오픈·진행 상태에서만 유형을 바꿀 수 있습니다. 진행으로 돌린 뒤 수정하세요."
            }));
        }
        if (typed || before_type != "general") && state != before_state && !typed_transition_allowed(before_state, state) {
            return Err(WorkBoardError::new("이 유형의 상태 전이가 허용되지 않습니다."));
        }
    }
    if typed
        && reviewing
        && (!truthy(next.get("completionCriteria"))
            || !truthy(next.get("completionEvidence"))
            || !truthy(next.get("completedById")))
    {
        return Err(WorkBoardError::new("검토·완료에는 완료 조건·결과 근거·완료자가 필요합니다."));
    }
    if typed && matches!(state, "deferred" | "canceled") && !truthy(next.get("resolutionReason")) {
        return Err(WorkBoardError::new("대기·취소에는 사유가 필요합니다."));
    }
    let user_review = str_of(next, "userReview").unwrap_or_default();
    if user_review != "none" {
        let allowed: &[&str] = if user_review == "changes_requested" { &["review", "in_progress"] } else { &["review", "done"] };
        if !allowed.contains(&state) || !truthy(next.get("userReviewNote")) {
            return Err(WorkBoardError::new("사용자 검토 기록에는 검토 상태와 메모가 필요합니다."));
        }
        let before_review = before.and_then(|before| str_of(before, "userReview")).unwrap_or("none");
        if user_review != before_review {
            let Some(actor) = str_of(next, "reportedActorId").filter(|actor| !actor.is_empty()) else {
                return Err(WorkBoardError::new("사용자 검토 결과 변경에는 기록자 신고값이 필요합니다."));
            };
            if actor != "user" {
                let found: Option<String> = sqlx::query_scalar("SELECT id FROM characters WHERE id = $1")
                    .bind(actor)
                    .fetch_optional(&mut *connection)
                    .await?;
                if found.is_none() {
                    return Err(WorkBoardError::new("검토 기록자는 사용자 또는 등록된 직원이어야 합니다."));
                }
            }
        }
    }
    if ticket_type != "operations" && !is_null(next, "operationImpact") {
        return Err(WorkBoardError::new("operationImpact는 운영 유형에서만 사용합니다."));
    }
    if ticket_type == "operations" && reviewing && is_null(next, "operationImpact") {
        return Err(WorkBoardError::new("운영 티켓은 내부·외부 영향 범위를 정해야 합니다."));
    }
    if ticket_type != "verification" && (!is_null(next, "targetTicketId") || !is_null(next, "verificationVerdict")) {
        return Err(WorkBoardError::new("검증 대상과 판정은 독립검증 유형에서만 사용합니다."));
    }
    let mut target = None;
    if ticket_type == "verification" && !is_null(next, "targetTicketId") {
        let target_id = str_of(next, "targetTicketId").unwrap_or_default();
        if Some(target_id) == ticket_id {
            return Err(WorkBoardError::new("자기 티켓은 검증 대상이 될 수 없습니다."));
        }
        let row: Option<(String, String, Option<String>, Option<String>, String)> = sqlx::query_as(
            "SELECT id::text, project_id::text, assignee_id, completed_by_id, state
       FROM work_board_tickets WHERE id = $1::uuid FOR UPDATE",
        )
        .bind(target_id)
        .fetch_optional(&mut *connection)
        .await?;
        match row {
            Some((id, project_id, assignee_id, completed_by_id, state)) if project_id == project.id => {
                target = Some(VerificationTarget { id, assignee_id, completed_by_id, state });
            }
            _ => return Err(WorkBoardError::new("검증 대상은 같은 프로젝트의 티켓이어야 합니다.")),
        }
    }
    if ticket_type == "verification" && reviewing && target.is_none() {
        return Err(WorkBoardError::new("독립검증 티켓에는 검증 대상이 필요합니다."));
    }
    if state == "done" && typed {
        if !matches!(before.and_then(|before| str_of(before, "state")), Some("review" | "done")) {
            return Err(WorkBoardError::new("완료 전에 검토 상태를 거쳐야 합니다."));
        }
        if truthy(next.get("decisionPending")) {
            return Err(WorkBoardError::new("사용자 결정 대기 중에는 완료할 수 없습니다."));
        }
        let needs_approval = matches!(ticket_type, "planning" | "content")
            || (ticket_type == "operations" && str_of(next, "operationImpact") == Some("external"));
        if needs_approval && user_review != "approved" {
            return Err(WorkBoardError::new("이 유형의 완료에는 사용자 검토 승인 기록이 필요합니다."));
        }
        if ticket_type == "verification" {
            if !truthy(next.get("verificationVerdict")) {
                return Err(WorkBoardError::new("독립검증 완료에는 합격·불합격 판정이 필요합니다."));
            }
            if let Some(target) = &target {
                if !matches!(target.state.as_str(), "review" | "done") {
                    return Err(WorkBoardError::new("검증 대상은 검토 또는 완료 상태여야 합니다."));
                }
                // Node의 [담당자, 완료자].includes(완료자). null끼리도 같다고 본다.
                let completed_by = str_of(next, "completedById");
                if target.assignee_id.as_deref() == completed_by || target.completed_by_id.as_deref() == completed_by {
                    return Err(WorkBoardError::new("검증 완료자는 대상 티켓의 담당자·완료자와 달라야 합니다."));
                }
            }
        }
    }
    require_delivery_evidence(project, next)?;
    Ok(target)
}

async fn require_project_tickets(
    connection: &mut PgConnection,
    project_id: &str,
    ids: &[String],
    field: &str,
) -> BoardResult<()> {
    if ids.is_empty() {
        return Ok(());
    }
    let found: i64 =
        sqlx::query_scalar("SELECT count(*) FROM work_board_tickets WHERE project_id = $1::uuid AND id = ANY($2::uuid[])")
            .bind(project_id)
            .bind(ids)
            .fetch_one(&mut *connection)
            .await?;
    if found != ids.len() as i64 {
        return Err(WorkBoardError::new(format!("{field}는 같은 프로젝트의 티켓이어야 합니다.")));
    }
    Ok(())
}

async fn require_project_goals(connection: &mut PgConnection, project_id: &str, ids: &[String]) -> BoardResult<()> {
    if ids.is_empty() {
        return Ok(());
    }
    let found: i64 =
        sqlx::query_scalar("SELECT count(*) FROM work_board_goals WHERE project_id = $1::uuid AND id = ANY($2::uuid[])")
            .bind(project_id)
            .bind(ids)
            .fetch_one(&mut *connection)
            .await?;
    if found != ids.len() as i64 {
        return Err(WorkBoardError::new("goalIds는 같은 프로젝트의 목표여야 합니다."));
    }
    Ok(())
}

fn ids_of(value: &Object, key: &str) -> Option<Vec<String>> {
    value
        .get(key)
        .and_then(Value::as_array)
        .map(|items| items.iter().filter_map(|item| item.as_str().map(str::to_string)).collect())
}

async fn validate_ticket_plan(
    connection: &mut PgConnection,
    project_id: &str,
    ticket_id: &str,
    value: &Object,
) -> BoardResult<()> {
    if let Some(parent) = str_of(value, "parentTicketId") {
        if parent == ticket_id {
            return Err(WorkBoardError::new("티켓은 자신의 부모가 될 수 없습니다."));
        }
        require_project_tickets(connection, project_id, &[parent.to_string()], "parentTicketId").await?;
        let rows: Vec<(String, Option<String>)> =
            sqlx::query_as("SELECT id::text, parent_ticket_id::text FROM work_board_tickets WHERE project_id = $1::uuid")
                .bind(project_id)
                .fetch_all(&mut *connection)
                .await?;
        let mut parents: HashMap<String, Option<String>> = rows.into_iter().collect();
        parents.insert(ticket_id.to_string(), Some(parent.to_string()));
        let mut visited = HashSet::new();
        let mut cursor = Some(parent.to_string());
        while let Some(current) = cursor {
            if current == ticket_id || visited.contains(&current) {
                return Err(WorkBoardError::new("WBS 부모 관계에 순환이 생깁니다."));
            }
            cursor = parents.get(&current).cloned().flatten();
            visited.insert(current);
        }
    }
    if let Some(predecessors) = ids_of(value, "predecessorIds") {
        if predecessors.iter().any(|id| id == ticket_id) {
            return Err(WorkBoardError::new("티켓은 자신의 선행 작업이 될 수 없습니다."));
        }
        require_project_tickets(connection, project_id, &predecessors, "predecessorIds").await?;
        let rows: Vec<(String, String)> = sqlx::query_as(
            "SELECT ticket_id::text, predecessor_id::text FROM work_board_ticket_dependencies WHERE project_id = $1::uuid",
        )
        .bind(project_id)
        .fetch_all(&mut *connection)
        .await?;
        let mut graph: HashMap<String, Vec<String>> = HashMap::new();
        for (ticket, predecessor) in rows {
            graph.entry(ticket).or_default().push(predecessor);
        }
        graph.insert(ticket_id.to_string(), predecessors.clone());
        let mut seen = HashSet::new();
        let mut pending = predecessors;
        while let Some(current) = pending.pop() {
            if current == ticket_id {
                return Err(WorkBoardError::new("선행 관계에 순환이 생깁니다."));
            }
            if !seen.insert(current.clone()) {
                continue;
            }
            pending.extend(graph.get(&current).cloned().unwrap_or_default());
        }
    }
    if let Some(goals) = ids_of(value, "goalIds") {
        require_project_goals(connection, project_id, &goals).await?;
    }
    Ok(())
}

async fn replace_ticket_links(
    connection: &mut PgConnection,
    project_id: &str,
    ticket_id: &str,
    value: &Object,
) -> BoardResult<()> {
    if let Some(predecessors) = ids_of(value, "predecessorIds") {
        sqlx::query("DELETE FROM work_board_ticket_dependencies WHERE ticket_id = $1::uuid")
            .bind(ticket_id)
            .execute(&mut *connection)
            .await?;
        for predecessor in predecessors {
            sqlx::query(
                "INSERT INTO work_board_ticket_dependencies (ticket_id, predecessor_id, project_id)
         VALUES ($1::uuid, $2::uuid, $3::uuid)",
            )
            .bind(ticket_id)
            .bind(predecessor)
            .bind(project_id)
            .execute(&mut *connection)
            .await?;
        }
    }
    if let Some(goals) = ids_of(value, "goalIds") {
        sqlx::query("DELETE FROM work_board_goal_tickets WHERE ticket_id = $1::uuid")
            .bind(ticket_id)
            .execute(&mut *connection)
            .await?;
        for goal in goals {
            sqlx::query(
                "INSERT INTO work_board_goal_tickets (ticket_id, goal_id, project_id)
         VALUES ($1::uuid, $2::uuid, $3::uuid)",
            )
            .bind(ticket_id)
            .bind(goal)
            .bind(project_id)
            .execute(&mut *connection)
            .await?;
        }
    }
    if let Some(records) = ids_of(value, "workRecordIds") {
        if !records.is_empty() {
            let found: i64 = sqlx::query_scalar("SELECT count(*) FROM work_records WHERE id = ANY($1::uuid[])")
                .bind(&records)
                .fetch_one(&mut *connection)
                .await?;
            if found != records.len() as i64 {
                return Err(WorkBoardError::new("workRecordIds에 없는 업무 기록이 있습니다."));
            }
        }
        sqlx::query("DELETE FROM work_board_ticket_records WHERE ticket_id = $1::uuid")
            .bind(ticket_id)
            .execute(&mut *connection)
            .await?;
        for record in records {
            sqlx::query("INSERT INTO work_board_ticket_records (ticket_id, work_record_id) VALUES ($1::uuid, $2::uuid)")
                .bind(ticket_id)
                .bind(record)
                .execute(&mut *connection)
                .await?;
        }
    }
    Ok(())
}

pub async fn create_ticket(connection: &mut PgConnection, body: &Value) -> BoardResult<Value> {
    let value = normalize_ticket_input(body, false)?;
    let project_id = str_of(&value, "projectId").unwrap_or_default().to_string();
    let project = lock_project(connection, &project_id).await?;
    validate_lifecycle(connection, &project, None, &value, None).await?;
    insert_ticket(connection, &project_id, &value).await.map(Value::Object).map_err(|error| {
        if is_database_code(&error, "23503") { WorkBoardError::new("프로젝트, 담당자 또는 연결 대상이 없습니다.") } else { error }
    })
}

async fn insert_ticket(connection: &mut PgConnection, project_id: &str, value: &Object) -> BoardResult<Object> {
    let field = |key: &str| str_of(value, key);
    // user_reviewed_at은 Node의 new Date()처럼 지금 시각을 밀리초까지 넣는다.
    let id: String = sqlx::query_scalar(
        "INSERT INTO work_board_tickets
       (project_id, title, description, assignee_id, completed_by_id, verified_by_id, state, due_date,
        completion_criteria, parent_ticket_id, decision_pending, pushed_commit_sha,
        ticket_type, completion_evidence, user_review, user_review_note, user_reviewed_at,
        target_ticket_id, verification_verdict, operation_impact, resolution_reason)
       VALUES ($1::uuid, $2, $3, $4, $5, $6, $7, $8::date, $9, $10::uuid, $11, $12,
               $13, $14, $15, $16,
               CASE WHEN $15 = 'none' THEN NULL ELSE date_trunc('milliseconds', clock_timestamp()) END,
               $17::uuid, $18, $19, $20) RETURNING id::text",
    )
    .bind(project_id)
    .bind(field("title"))
    .bind(field("description"))
    .bind(field("assigneeId"))
    .bind(field("completedById"))
    .bind(field("verifiedById"))
    .bind(field("state"))
    .bind(field("dueDate"))
    .bind(field("completionCriteria"))
    .bind(field("parentTicketId"))
    .bind(value.get("decisionPending").and_then(Value::as_bool).unwrap_or(false))
    .bind(field("pushedCommitSha"))
    .bind(field("ticketType"))
    .bind(field("completionEvidence"))
    .bind(field("userReview"))
    .bind(field("userReviewNote"))
    .bind(field("targetTicketId"))
    .bind(field("verificationVerdict"))
    .bind(field("operationImpact"))
    .bind(field("resolutionReason"))
    .fetch_one(&mut *connection)
    .await?;
    validate_ticket_plan(connection, project_id, &id, value).await?;
    replace_ticket_links(connection, project_id, &id, value).await?;
    write_change_activity(connection, &id, "created", &json!({}), field("reportedActorId")).await?;
    read_ticket(connection, &id).await
}

fn changed_fields<'a>(before: &Object, after: &Object, fields: impl IntoIterator<Item = &'a str>) -> Object {
    let mut changes = Object::new();
    for field in fields {
        if before.get(field) != after.get(field) {
            changes.insert(field.into(), json!({ "before": before.get(field), "after": after.get(field) }));
        }
    }
    changes
}

pub async fn update_ticket(connection: &mut PgConnection, id: &str, body: &Value) -> BoardResult<Value> {
    require_uuid(id, "ticketId")?;
    let mut value = normalize_ticket_input(body, true)?;
    let project_id: Option<String> = sqlx::query_scalar("SELECT project_id::text FROM work_board_tickets WHERE id = $1::uuid")
        .bind(id)
        .fetch_optional(&mut *connection)
        .await?;
    let Some(project_id) = project_id else {
        return Err(WorkBoardError::with_status("티켓이 없습니다.", 404));
    };
    let project = lock_project(connection, &project_id).await?;
    let before = read_ticket(connection, id).await?;
    let review_material_changed =
        ["description", "completionCriteria", "completionEvidence", "ticketType", "operationImpact", "targetTicketId"]
            .iter()
            .any(|field| value.get(*field).is_some_and(|changed| before.get(*field) != Some(changed)));
    let requests_changes = str_of(&value, "userReview") == Some("changes_requested");
    if (str_of(&before, "state") == Some("review") && str_of(&value, "state") == Some("in_progress") && !requests_changes)
        || (str_of(&before, "userReview") == Some("approved") && review_material_changed && !requests_changes)
    {
        value.insert("userReview".into(), "none".into());
        value.insert("userReviewNote".into(), "".into());
    }
    if str_of(&value, "userReview") == Some("none") {
        value.insert("userReviewNote".into(), "".into());
    }
    let mut next = before.clone();
    for (key, field) in &value {
        next.insert(key.clone(), field.clone());
    }
    let target = validate_lifecycle(connection, &project, Some(&before), &next, Some(id)).await?;
    validate_ticket_plan(connection, &project_id, id, &value).await?;
    apply_ticket_update(connection, &project_id, id, &value, &before, &next, target).await.map(Value::Object).map_err(
        |error| if is_database_code(&error, "23503") { WorkBoardError::new("담당자가 없습니다.") } else { error },
    )
}

async fn apply_ticket_update(
    connection: &mut PgConnection,
    project_id: &str,
    id: &str,
    value: &Object,
    before: &Object,
    next: &Object,
    target: Option<VerificationTarget>,
) -> BoardResult<Object> {
    let (mut sets, binds) = assignments(value, ticket_column);
    if value.contains_key("userReview") && value.get("userReview") != before.get("userReview") {
        sets.push(if str_of(value, "userReview") == Some("none") {
            "user_reviewed_at = NULL".to_string()
        } else {
            "user_reviewed_at = date_trunc('milliseconds', clock_timestamp())".to_string()
        });
    }
    let prefix = if sets.is_empty() { String::new() } else { format!("{}, ", sets.join(", ")) };
    let sql = format!("UPDATE work_board_tickets SET {prefix}updated_at = now() WHERE id = $1::uuid RETURNING id::text");
    let mut query = sqlx::query_scalar::<_, String>(AssertSqlSafe(sql)).bind(id);
    for bind in binds {
        query = query.bind(bind);
    }
    if query.fetch_optional(&mut *connection).await?.is_none() {
        return Err(WorkBoardError::with_status("티켓이 없습니다.", 404));
    }
    replace_ticket_links(connection, project_id, id, value).await?;
    let reported_actor_id = str_of(value, "reportedActorId");
    if str_of(before, "state") != Some("done")
        && str_of(next, "state") == Some("done")
        && str_of(next, "ticketType") == Some("verification")
        && let Some(target) = target
    {
        let target_before = read_ticket(connection, &target.id).await?;
        if str_of(next, "verificationVerdict") == Some("fail") {
            sqlx::query(
                "UPDATE work_board_tickets SET state = 'in_progress', completed_by_id = NULL,
           verified_by_id = NULL, completion_evidence = '', pushed_commit_sha = NULL,
           user_review = 'none', user_review_note = '', user_reviewed_at = NULL,
           decision_pending = false, updated_at = now() WHERE id = $1::uuid",
            )
            .bind(&target.id)
            .execute(&mut *connection)
            .await?;
        } else {
            sqlx::query("UPDATE work_board_tickets SET verified_by_id = $2, updated_at = now() WHERE id = $1::uuid")
                .bind(&target.id)
                .bind(str_of(next, "completedById"))
                .execute(&mut *connection)
                .await?;
        }
        let target_after = read_ticket(connection, &target.id).await?;
        let target_fields = ["state", "completedById", "verifiedById", "completionEvidence", "pushedCommitSha",
            "userReview", "userReviewNote", "userReviewedAt", "decisionPending"];
        let changes = changed_fields(&target_before, &target_after, target_fields);
        if !changes.is_empty() {
            write_change_activity(connection, &target.id, "changed", &Value::Object(changes), reported_actor_id).await?;
        }
    }
    let after = read_ticket(connection, id).await?;
    let changes = changed_fields(before, &after, value.keys().map(String::as_str).filter(|key| *key != "reportedActorId"));
    if !changes.is_empty() {
        write_change_activity(connection, id, "changed", &Value::Object(changes), reported_actor_id).await?;
    }
    Ok(after)
}

pub async fn create_goal(connection: &mut PgConnection, body: &Value) -> BoardResult<Value> {
    let value = normalize_goal_input(body, false)?;
    lock_project(connection, str_of(&value, "projectId").unwrap_or_default()).await?;
    let field = |key: &str| str_of(&value, key);
    Ok(sqlx::query_scalar(concat!(
        "WITH created AS (INSERT INTO work_board_goals
     (project_id, horizon, period_start, title, description, metric_name,
      metric_unit, target_value, actual_value)
     VALUES ($1::uuid, $2, $3::date, $4, $5, $6, $7, $8::numeric, $9::numeric)
     RETURNING ",
        goal_columns!(),
        ") SELECT row_to_json(created) FROM created"
    ))
    .bind(field("projectId"))
    .bind(field("horizon"))
    .bind(field("periodStart"))
    .bind(field("title"))
    .bind(field("description"))
    .bind(field("metricName"))
    .bind(field("metricUnit"))
    .bind(field("targetValue"))
    .bind(field("actualValue"))
    .fetch_one(&mut *connection)
    .await?)
}

pub async fn update_goal(connection: &mut PgConnection, id: &str, body: &Value) -> BoardResult<Value> {
    require_uuid(id, "goalId")?;
    let value = normalize_goal_input(body, true)?;
    let existing: Option<Value> = sqlx::query_scalar(
        r#"SELECT row_to_json(x) FROM (SELECT project_id AS "projectId", horizon,
      period_start::text AS "periodStart", metric_name AS "metricName",
      target_value::text AS "targetValue", actual_value::text AS "actualValue"
     FROM work_board_goals WHERE id = $1::uuid) AS x"#,
    )
    .bind(id)
    .fetch_optional(&mut *connection)
    .await?;
    let Some(Value::Object(mut merged)) = existing else {
        return Err(WorkBoardError::with_status("목표가 없습니다.", 404));
    };
    let project_id = str_of(&merged, "projectId").unwrap_or_default().to_string();
    lock_project(connection, &project_id).await?;
    for (key, field) in &value {
        merged.insert(key.clone(), field.clone());
    }
    validate_goal_period(str_of(&merged, "horizon").unwrap_or_default(), str_of(&merged, "periodStart").unwrap_or_default())?;
    if (!is_null(&merged, "targetValue") || !is_null(&merged, "actualValue")) && !truthy(merged.get("metricName")) {
        return Err(WorkBoardError::new("수치 목표에는 metricName이 필요합니다."));
    }
    let (sets, binds) = assignments(&value, goal_column);
    let sql = format!(
        "WITH updated AS (UPDATE work_board_goals SET {}, updated_at = now()
     WHERE id = $1::uuid RETURNING {}) SELECT row_to_json(updated) FROM updated",
        sets.join(", "),
        goal_columns!()
    );
    let mut query = sqlx::query_scalar::<_, Value>(AssertSqlSafe(sql)).bind(id);
    for bind in binds {
        query = query.bind(bind);
    }
    // Node는 행이 없으면 goal을 빼고 보낸다. 잠근 뒤라 일어나지 않지만 null로 둔다.
    Ok(query.fetch_optional(&mut *connection).await?.unwrap_or(Value::Null))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rejects<T: fmt::Debug>(result: BoardResult<T>) -> String {
        match result {
            Err(WorkBoardError::Rejected { status: 400, message }) => message,
            other => panic!("400 거부가 아님: {other:?}"),
        }
    }

    const PROJECT: &str = "3f2a1c4e-5b6d-4e7f-8a9b-0c1d2e3f4a5b";

    #[test]
    fn 프로젝트_티켓_입력은_상태_날짜_필드와_담당자를_검증한다() {
        assert_eq!(
            Value::Object(normalize_project_input(&json!({ "projectKey": "toss-trading", "title": " Toss " }), false).unwrap()),
            json!({ "projectKey": "toss-trading", "title": "Toss", "description": "" })
        );
        rejects(normalize_project_input(&json!({ "projectKey": "TOSS", "title": "Toss" }), false));
        rejects(normalize_project_input(&json!({ "projectKey": "toss-trading", "title": " " }), false));
        rejects(normalize_project_input(&json!({ "title": "X", "extra": true }), true));
        let ticket = normalize_ticket_input(&json!({ "projectId": PROJECT, "title": " 확인 " }), false).unwrap();
        assert_eq!(ticket["title"], "확인");
        assert_eq!(ticket["state"], "open");
        for field in ["pushedCommitSha", "dueDate", "assigneeId", "completedById", "verifiedById"] {
            assert_eq!(ticket[field], Value::Null, "{field}");
        }
        assert_eq!(ticket["ticketType"], "general");
        assert_eq!(ticket["userReview"], "none");
        let patch = |body: Value| normalize_ticket_input(&body, true).map(Value::Object);
        assert_eq!(
            patch(json!({ "completedById": "left-man", "verifiedById": "left-woman" })).unwrap(),
            json!({ "completedById": "left-man", "verifiedById": "left-woman" })
        );
        assert_eq!(
            patch(json!({ "completedById": null, "verifiedById": null })).unwrap(),
            json!({ "completedById": null, "verifiedById": null })
        );
        rejects(patch(json!({ "completedById": 5 })));
        for value in [json!("2026-02-30"), json!("2026-13-01"), json!("내일"), json!(20260925)] {
            rejects(patch(json!({ "dueDate": value })));
        }
        assert_eq!(patch(json!({ "dueDate": null, "assigneeId": null })).unwrap(), json!({ "dueDate": null, "assigneeId": null }));
        for state in ["active", "todo", "blocked"] {
            rejects(patch(json!({ "state": state })));
        }
        for state in STATES {
            assert_eq!(patch(json!({ "state": state })).unwrap()["state"], state);
        }
        let sha = "a".repeat(40);
        assert_eq!(patch(json!({ "pushedCommitSha": sha })).unwrap(), json!({ "pushedCommitSha": sha }));
        for sha in [json!("abc"), json!("A".repeat(40)), json!("g".repeat(40)), json!(42)] {
            rejects(patch(json!({ "pushedCommitSha": sha })));
        }
        rejects(patch(json!({ "state": null })));
        rejects(patch(json!({ "projectId": PROJECT })));
        assert_eq!(ticket["decisionPending"], false);
        assert_eq!(patch(json!({ "decisionPending": true })).unwrap(), json!({ "decisionPending": true }));
        rejects(patch(json!({ "decisionPending": "true" })));
        assert_eq!(
            patch(json!({ "ticketType": "pretest", "userReview": "approved", "userReviewNote": "사용자 화면 확인" })).unwrap(),
            json!({ "ticketType": "pretest", "userReview": "approved", "userReviewNote": "사용자 화면 확인" })
        );
        for bad in [
            json!({ "ticketType": "bugfix" }),
            json!({ "userReview": "yes" }),
            json!({ "verificationVerdict": "maybe" }),
            json!({ "operationImpact": "unknown" }),
            json!({ "targetTicketId": "not-a-uuid" }),
        ] {
            rejects(patch(bad));
        }
    }

    #[test]
    fn 진행_기록은_본문과_신고된_작성자만_받는다() {
        assert_eq!(
            Value::Object(
                normalize_activity_input(&json!({ "kind": "progress", "body": " 진행 ", "reportedActorId": "left-woman" }))
                    .unwrap()
            ),
            json!({ "kind": "progress", "body": "진행", "reportedActorId": "left-woman" })
        );
        for bad in [
            json!({ "kind": "changed", "body": "수정" }),
            json!({ "kind": "progress", "body": " " }),
            json!({ "kind": "decision", "body": "x", "extra": true }),
        ] {
            rejects(normalize_activity_input(&bad));
        }
    }

    #[test]
    fn 주간_월간_목표는_명시한_기간과_문자열_수치만_받는다() {
        let goal = |body: Value| normalize_goal_input(&body, false);
        let weekly =
            goal(json!({ "projectId": PROJECT, "horizon": "weekly", "periodStart": "2026-09-21", "title": "검증 완료" })).unwrap();
        assert_eq!(weekly["targetValue"], Value::Null);
        assert_eq!(weekly["actualValue"], Value::Null);
        rejects(goal(json!({ "projectId": PROJECT, "horizon": "weekly", "periodStart": "2026-09-22", "title": "잘못된 시작" })));
        rejects(goal(json!({ "projectId": PROJECT, "horizon": "monthly", "periodStart": "2026-09-02", "title": "잘못된 시작" })));
        rejects(goal(json!({ "projectId": PROJECT, "horizon": "monthly", "periodStart": "2026-10-01", "title": "수치",
            "metricName": "처리 건수", "targetValue": 10 })));
        rejects(goal(json!({ "projectId": PROJECT, "horizon": "monthly", "periodStart": "2026-10-01", "title": "수치",
            "targetValue": "10" })));
        let patch = |body: Value| normalize_ticket_input(&body, true).map(Value::Object);
        assert_eq!(
            patch(json!({ "parentTicketId": null, "predecessorIds": [], "goalIds": [] })).unwrap(),
            json!({ "parentTicketId": null, "predecessorIds": [], "goalIds": [] })
        );
        rejects(patch(json!({ "predecessorIds": [PROJECT, PROJECT] })));
        rejects(patch(json!({ "goalIds": null })));
        rejects(patch(json!({ "workRecordIds": [PROJECT, PROJECT] })));
        assert_eq!(patch(json!({ "workRecordIds": [] })).unwrap(), json!({ "workRecordIds": [] }));
    }

    #[test]
    fn js_문자열_규칙과_오류_순서를_따른다() {
        // JS trim은 U+FEFF를 지우고 U+0085는 남긴다. 길이는 UTF-16 단위다.
        assert_eq!(text(Some(&json!("\u{feff} 제목 \u{3000}")), "title", 160, true).unwrap(), "제목");
        assert_eq!(text(Some(&json!("\u{85}")), "title", 160, true).unwrap(), "\u{85}");
        assert!(text(Some(&json!("😀".repeat(80))), "title", 160, true).is_ok());
        assert_eq!(rejects(text(Some(&json!("😀".repeat(81))), "title", 160, true)), "title 값이 올바르지 않습니다.");
        // 알 수 없는 필드가 먼저, 생성 때는 projectId가 먼저다.
        assert_eq!(rejects(normalize_ticket_input(&json!({ "state": "x", "nope": 1 }), true)), "알 수 없는 필드: nope");
        assert_eq!(rejects(normalize_ticket_input(&json!({ "title": "" }), false)), "projectId는 UUID여야 합니다.");
        assert_eq!(rejects(normalize_ticket_input(&json!({}), true)), "수정할 필드가 없습니다.");
        assert_eq!(rejects(normalize_ticket_input(&json!([]), true)), "JSON 객체가 필요합니다.");
        // 기간 시작은 dueDate 문구를 그대로 쓴다.
        assert_eq!(
            rejects(normalize_goal_input(&json!({ "periodStart": "2026-1-1" }), true)),
            "dueDate는 YYYY-MM-DD 또는 null이어야 합니다."
        );
        assert_eq!(rejects(normalize_goal_input(&json!({ "periodStart": "2025-02-29" }), true)), "dueDate가 유효한 날짜가 아닙니다.");
        assert!(normalize_goal_input(&json!({ "periodStart": "2024-02-29" }), true).is_ok());
        assert_eq!(rejects(decimal_text(Some(&json!("0.000")), "targetValue", true)), "targetValue는 0보다 커야 합니다.");
        assert_eq!(decimal_text(Some(&json!("0.000")), "actualValue", false).unwrap(), "0.000");
        assert!(decimal_text(Some(&json!("1.")), "actualValue", false).is_err());
        assert_eq!(utc_weekday("2026-09-21"), 1);
        assert_eq!(utc_weekday("1970-01-01"), 4);
        assert_eq!(utc_weekday("2000-02-29"), 2);
    }

    #[test]
    fn 변경_요약은_라벨과_180자_자르기를_쓴다() {
        let long = "가".repeat(200);
        let row = json!({ "kind": "changed", "changes": {
            "state": { "before": "open", "after": "deferred" },
            "description": { "before": "", "after": long },
            "custom": { "before": null, "after": [1] }
        } });
        let summary = activity_summary(row.as_object().unwrap());
        let expected_long = format!("\"{}…", "가".repeat(179));
        assert_eq!(summary, format!("상태: \"open\" → \"deferred\" · 설명: \"\" → {expected_long} · custom: null → [1]"));
        assert_eq!(activity_summary(json!({ "kind": "created", "body": "" }).as_object().unwrap()), "티켓 생성");
        assert_eq!(activity_summary(json!({ "kind": "progress", "body": "메모" }).as_object().unwrap()), "메모");
    }
}
