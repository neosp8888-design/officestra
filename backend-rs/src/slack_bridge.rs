// Slack 소켓 모드 메시지를 OFFICESTRA 직원 업무와 실시간 결과로 잇는다. Node slack-bridge.mjs와 Bolt 5.0.0의 처리 순서를 따른다.

pub mod socket;
pub mod web;

#[cfg(test)]
mod tests;

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::FutureExt;
use futures_util::future::{BoxFuture, Shared};
use serde_json::{Map, Value, json};
use sqlx::{PgPool, Row};

use crate::js::{encode_uri_component, is_js_whitespace, js_string, js_trim, js_truthy, parse_js_json, pg_text, utf16_prefix, utf16_suffix};
use crate::rag::database_message;
use crate::roleplay::js_regex::JsRegExp;
use socket::{Acker, Envelope, SocketMode, SocketTimings};
use web::{SOCKET_RETRIES, SlackError, TEN_RETRIES, WebClient};

const TERMINAL_STATUSES: [&str; 3] = ["completed", "failed", "interrupted"];
const STATUS_TEXT_LIMIT: usize = 2_700;
pub const MESSAGE_TEXT_LIMIT: usize = 35_000;
pub const SLACK_API_URL: &str = "https://slack.com/api/";
const SELECT_ACTION_PREFIX: &str = "officestra.select-character.";

/// Slack 연결 설정. 토큰이 들어 있으므로 Debug를 두지 않는다.
pub struct SlackConfiguration {
    pub bot_token: String,
    pub app_token: String,
    pub allowed_user_ids: Vec<String>,
    pub allowed_team_ids: Vec<String>,
    pub file_path: String,
}

/// `readSlackConfiguration`. 파일 값 위에 비어 있지 않은 환경 변수를 덮는다.
/// file_path가 없으면 OFFICE_SLACK_ENV_FILE(빈 값 포함), 그다음 ~/.officestra/slack.env다.
pub fn read_slack_configuration(environment: &[(String, String)], file_path: Option<&str>) -> Result<SlackConfiguration, String> {
    let lookup = |key: &str| environment.iter().find(|(name, _)| name == key).map(|(_, value)| value.as_str());
    let file_path = match file_path {
        Some(path) => path.to_string(),
        None => lookup("OFFICE_SLACK_ENV_FILE")
            .map(str::to_string)
            .unwrap_or_else(|| crate::js::join_path(&[&crate::local_artifacts::home_directory(), ".officestra", "slack.env"])),
    };
    let mut merged = if !file_path.is_empty() && std::path::Path::new(&file_path).exists() {
        let bytes = std::fs::read(&file_path).map_err(|error| match error.kind() {
            std::io::ErrorKind::IsADirectory => "EISDIR: illegal operation on a directory, read".to_string(),
            _ => crate::js::node_fs_message(&error, "open", &file_path),
        })?;
        parse_environment_file(&String::from_utf8_lossy(&bytes))
    } else {
        Vec::new()
    };
    for (key, value) in environment {
        if !js_trim(value).is_empty() {
            set_entry(&mut merged, key, value);
        }
    }
    let get = |key: &str| merged.iter().find(|(name, _)| name == key).map(|(_, value)| value.as_str());
    Ok(SlackConfiguration {
        bot_token: get("SLACK_BOT_TOKEN").map(js_trim).unwrap_or_default().to_string(),
        app_token: get("SLACK_APP_TOKEN").map(js_trim).unwrap_or_default().to_string(),
        allowed_user_ids: identifier_set(get("OFFICE_SLACK_ALLOWED_USER_IDS")),
        allowed_team_ids: identifier_set(get("OFFICE_SLACK_ALLOWED_TEAM_IDS")),
        file_path,
    })
}

/// JS 객체에 값 넣기. 있던 키는 자리를 지키고, `__proto__`에 문자열을 넣는 것은 무시된다.
fn set_entry(entries: &mut Vec<(String, String)>, key: &str, value: &str) {
    if key == "__proto__" {
        return;
    }
    match entries.iter_mut().find(|(name, _)| name == key) {
        Some(entry) => entry.1 = value.to_string(),
        None => entries.push((key.to_string(), value.to_string())),
    }
}

/// `parseEnvironmentFile`. 빈 줄·# 주석·= 없는 줄은 건너뛰고, 같은 따옴표로 감싼 값은 벗긴다.
pub fn parse_environment_file(source: &str) -> Vec<(String, String)> {
    let mut result = Vec::new();
    for raw_line in source.split('\n') {
        let line = js_trim(raw_line);
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some(separator) = line.find('=').filter(|separator| *separator >= 1) else { continue };
        let key = js_trim(&line[..separator]);
        let mut value = js_trim(&line[separator + 1..]);
        if value.len() >= 2 && ((value.starts_with('"') && value.ends_with('"')) || (value.starts_with('\'') && value.ends_with('\''))) {
            value = &value[1..value.len() - 1];
        }
        set_entry(&mut result, key, value);
    }
    result
}

/// `identifierSet`. 공백·쉼표로 나눈 값을 넣은 순서대로 중복 없이 둔다.
fn identifier_set(value: Option<&str>) -> Vec<String> {
    let mut items: Vec<String> = Vec::new();
    for item in value.unwrap_or_default().split(|c: char| c == ',' || is_js_whitespace(c)) {
        if !item.is_empty() && !items.iter().any(|existing| existing == item) {
            items.push(item.to_string());
        }
    }
    items
}

/// 값이 없거나 null이면 None(JS의 `??`가 건너뛰는 값).
fn present(value: Option<&Value>) -> Option<&Value> {
    value.filter(|value| !value.is_null())
}

/// `value?.key`. 앞이 없거나 null이면 undefined, 객체가 아니면 undefined다.
fn member<'a>(value: Option<&'a Value>, key: &str) -> Option<&'a Value> {
    present(value)?.as_object()?.get(key)
}

/// `value.key`. 앞이 undefined·null이면 Node와 같은 TypeError 문구다.
fn read<'a>(value: Option<&'a Value>, key: &str) -> Result<Option<&'a Value>, String> {
    match value {
        None => Err(format!("Cannot read properties of undefined (reading '{key}')")),
        Some(Value::Null) => Err(format!("Cannot read properties of null (reading '{key}')")),
        Some(Value::Object(fields)) => Ok(fields.get(key)),
        Some(_) => Ok(None),
    }
}

/// 템플릿 문자열 `${value}`. undefined는 "undefined"다.
fn template(value: Option<&Value>) -> String {
    value.map_or_else(|| "undefined".to_string(), js_string)
}

/// `===`. None은 undefined다. 객체·배열은 같은 참조가 아니므로 같지 않다.
fn strict_equal(left: Option<&Value>, right: Option<&Value>) -> bool {
    match (left, right) {
        (None, None) => true,
        (Some(Value::Null), Some(Value::Null)) => true,
        (Some(Value::Bool(left)), Some(Value::Bool(right))) => left == right,
        (Some(Value::Number(left)), Some(Value::Number(right))) => left.as_f64() == right.as_f64(),
        (Some(Value::String(left)), Some(Value::String(right))) => left == right,
        _ => false,
    }
}

fn pg(value: Option<&Value>) -> Option<String> {
    value.and_then(pg_text)
}

/// `employeeSelectionBlocks`. 앞 5명만 버튼으로 두고 현재 선택은 ✓와 primary로 표시한다.
pub fn employee_selection_blocks(characters: &[Value], selected_id: Option<&Value>) -> Value {
    let selected = selected_id.unwrap_or(&Value::Null);
    let buttons: Vec<Value> = characters
        .iter()
        .take(5)
        .map(|character| {
            let id = member(Some(character), "id");
            let chosen = strict_equal(id, Some(selected));
            let mut button = Map::new();
            button.insert("type".into(), json!("button"));
            button.insert("action_id".into(), json!(format!("{SELECT_ACTION_PREFIX}{}", template(id))));
            let label = format!("{}{}", if chosen { "✓ " } else { "" }, template(member(Some(character), "name")));
            button.insert("text".into(), json!({ "type": "plain_text", "text": label, "emoji": true }));
            if let Some(id) = id {
                button.insert("value".into(), id.clone());
            }
            if chosen {
                button.insert("style".into(), json!("primary"));
            }
            Value::Object(button)
        })
        .collect();
    json!([
        {
            "type": "section",
            "text": { "type": "mrkdwn", "text": "*OFFICESTRA 직원 선택*\n앞으로 보낼 업무를 담당할 직원을 선택하세요." },
        },
        { "type": "actions", "elements": buttons },
    ])
}

fn is_terminal(turn: &Value) -> bool {
    member(Some(turn), "status").and_then(Value::as_str).is_some_and(|status| TERMINAL_STATUSES.contains(&status))
}

/// 문자열 값을 JS trim한 결과. 문자열이 아니면 None이다.
fn trimmed(value: Option<&Value>) -> Option<&str> {
    present(value).and_then(Value::as_str).map(js_trim)
}

/// `renderSlackTurn`. 최근 활동 4개와 진행 중 응답 끝 1,200자를 2,700자 안에서 보인다.
pub fn render_slack_turn(turn: &Value) -> String {
    let name = present(member(Some(turn), "characterName")).map_or_else(|| "직원".to_string(), js_string);
    let label = slack_status_label(turn);
    let texts: Vec<&str> = member(Some(turn), "activities")
        .and_then(Value::as_array)
        .map(|activities| activities.iter().filter_map(|activity| trimmed(member(Some(activity), "text"))).filter(|text| !text.is_empty()).collect())
        .unwrap_or_default();
    let activities = texts[texts.len().saturating_sub(4)..].iter().map(|text| format!("• {text}")).collect::<Vec<_>>().join("\n");
    let partial = match trimmed(member(Some(turn), "response")) {
        Some(response) if !is_terminal(turn) && !response.is_empty() => format!("\n\n{}", utf16_suffix(response, 1_200)),
        _ => String::new(),
    };
    let details = [activities, partial].into_iter().filter(|part| !part.is_empty()).collect::<Vec<_>>().join("\n");
    let heading = format!("{label} *{}*", escape_slack_text(&name));
    let text = if details.is_empty() { heading } else { format!("{heading}\n{details}") };
    utf16_prefix(&text, STATUS_TEXT_LIMIT)
}

fn progress_blocks(turn: &Value) -> Value {
    json!([{ "type": "section", "text": { "type": "mrkdwn", "text": render_slack_turn(turn) } }])
}

fn slack_status_label(turn: &Value) -> String {
    if js_truthy(member(Some(turn), "needsInput")) {
        return "❓ 답변 필요".into();
    }
    match member(Some(turn), "status").and_then(Value::as_str) {
        Some("completed") => "✅ 완료".into(),
        Some("failed") => {
            let error = member(Some(turn), "errorMessage").filter(|error| js_truthy(Some(error)));
            format!("❌ 실패{}", error.map(|error| format!(" · {}", js_string(error))).unwrap_or_default())
        }
        Some("interrupted") => "⏹️ 중단".into(),
        Some("starting") => "⏳ 업무 시작".into(),
        _ => "⏳ 작업 중".into(),
    }
}

/// `stripSlackMention`. 봇 ID를 알면 그 멘션만, 모르면 모든 사용자 멘션을 빼고 공백을 하나로 줄인다.
pub fn strip_slack_mention(text: Option<&Value>, bot_user_id: Option<&Value>) -> String {
    let value = crate::js::js_string_or_empty(text);
    let removed = match bot_user_id.filter(|id| js_truthy(Some(id))) {
        Some(id) => value.replace(&format!("<@{}>", js_string(id)), " "),
        None => remove_user_mentions(&value),
    };
    let mut collapsed = String::with_capacity(removed.len());
    let mut in_space = false;
    for c in removed.chars() {
        if is_js_whitespace(c) {
            if !in_space {
                collapsed.push(' ');
            }
            in_space = true;
        } else {
            collapsed.push(c);
            in_space = false;
        }
    }
    js_trim(&collapsed).to_string()
}

/// `/<@[A-Z0-9]+>/g`를 공백 하나로 바꾼다.
fn remove_user_mentions(text: &str) -> String {
    let mut output = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find("<@") {
        let after = &rest[start + 2..];
        let length = after.bytes().take_while(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit()).count();
        if length > 0 && after.as_bytes().get(length) == Some(&b'>') {
            output.push_str(&rest[..start]);
            output.push(' ');
            rest = &after[length + 1..];
        } else {
            output.push_str(&rest[..start + 1]);
            rest = &rest[start + 1..];
        }
    }
    output.push_str(rest);
    output
}

fn trim_units(units: &[u16]) -> &[u16] {
    let space = |unit: &u16| char::from_u32(u32::from(*unit)).is_some_and(is_js_whitespace);
    let start = units.iter().position(|unit| !space(unit)).unwrap_or(units.len());
    let end = units.iter().rposition(|unit| !space(unit)).map_or(start, |end| end + 1);
    &units[start..end.max(start)]
}

/// `splitSlackMessage`. limit(UTF-16 단위)을 넘으면 limit×0.6 뒤의 마지막 줄바꿈, 없으면 limit에서 자른다.
pub fn split_slack_message(text: &str, limit: usize) -> Vec<String> {
    let units: Vec<u16> = text.encode_utf16().collect();
    let mut remaining = trim_units(&units);
    if remaining.is_empty() {
        return Vec::new();
    }
    let mut chunks = Vec::new();
    while remaining.len() > limit {
        let newline = remaining[..=limit.min(remaining.len() - 1)].iter().rposition(|unit| *unit == u16::from(b'\n'));
        let boundary = match newline {
            Some(newline) if newline as f64 > limit as f64 * 0.6 => newline,
            _ => limit,
        };
        chunks.push(String::from_utf16_lossy(trim_units(&remaining[..boundary])));
        remaining = trim_units(&remaining[boundary..]);
    }
    if !remaining.is_empty() {
        chunks.push(String::from_utf16_lossy(remaining));
    }
    chunks
}

/// `normalizeSlackMarkdown`. 제목 줄과 **굵게**를 Slack의 *굵게*로 바꾼다.
fn normalize_slack_markdown(text: &str) -> String {
    let headings = JsRegExp::new(r"^#{1,6}\s+(.+)$", "gm").map(|mut regex| regex.replace(text, "*$1*")).unwrap_or_else(|_| text.to_string());
    JsRegExp::new(r"\*\*(.+?)\*\*", "g").map(|mut regex| regex.replace(&headings, "*$1*")).unwrap_or(headings)
}

fn escape_slack_text(text: &str) -> String {
    text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

/// `escapeSlackText(value)`. undefined·null은 빈 문자열이다.
fn escape_value(value: Option<&Value>) -> String {
    escape_slack_text(&crate::js::js_string_or_empty(value))
}

/// `slackTeamID`. team_id, team.id, authorizations[0].team_id 순서이고 없으면 빈 문자열이다.
pub fn slack_team_id(body: &Value) -> Value {
    let first_authorization = member(Some(body), "authorizations").and_then(|authorizations| match authorizations {
        Value::Array(items) => items.first(),
        Value::Object(fields) => fields.get("0"),
        _ => None,
    });
    present(member(Some(body), "team_id"))
        .or_else(|| present(member(member(Some(body), "team"), "id")))
        .or_else(|| present(member(first_authorization, "team_id")))
        .cloned()
        .unwrap_or_else(|| json!(""))
}

/// `isAuthorizedSlackRequest`. 허용 사용자이고, 허용 팀 목록이 있으면 그 팀이어야 한다.
pub fn is_authorized_slack_request(configuration: &SlackConfiguration, body: &Value) -> bool {
    let user = present(member(Some(body), "user_id"))
        .or_else(|| present(member(member(Some(body), "user"), "id")))
        .or_else(|| present(member(member(Some(body), "event"), "user")));
    let allowed = |list: &[String], value: &Value| value.as_str().is_some_and(|value| list.iter().any(|item| item == value));
    let Some(user) = user.filter(|user| js_truthy(Some(user))) else { return false };
    allowed(&configuration.allowed_user_ids, user)
        && (configuration.allowed_team_ids.is_empty() || allowed(&configuration.allowed_team_ids, &slack_team_id(body)))
}

/// Web API 인자 객체. undefined인 값은 넣지 않는다(JS 객체 리터럴의 undefined가 본문에서 빠지는 것과 같다).
fn arguments(entries: Vec<(&str, Option<Value>)>) -> Value {
    let mut fields = Map::new();
    for (key, value) in entries {
        if let Some(value) = value {
            fields.insert(key.to_string(), value);
        }
    }
    Value::Object(fields)
}

async fn call(client: &WebClient, method: &str, arguments: Value) -> Result<Value, String> {
    client.api_call(method, arguments).await.map_err(|error| error.message())
}

fn type_error(message: String) -> String {
    format!("TypeError: {message}")
}

/// slack_threads에서 읽은 대화 연결.
#[derive(Clone, Debug, PartialEq)]
struct SlackThread {
    character_id: String,
    conversation_id: String,
}

/// 재시작 뒤 다시 지켜볼 업무(`pendingSlackTurnTargets`).
#[derive(Clone, Debug, PartialEq)]
struct PendingTarget {
    channel_id: String,
    thread_ts: String,
    message_ts: String,
    turn_id: String,
}

/// `recordSlackTurnLaunch` 인자. 백엔드 응답 값은 JS 값 그대로 받는다.
struct LaunchRecord<'a> {
    team_id: &'a Value,
    channel_id: &'a Value,
    thread_ts: &'a Value,
    conversation_id: Option<&'a Value>,
    turn_id: Option<&'a Value>,
    status_message_ts: Option<&'a Value>,
}

async fn claim_slack_event(pool: &PgPool, event_id: &Value) -> Result<bool, String> {
    let claimed = sqlx::query_scalar::<_, String>(
        "INSERT INTO slack_event_receipts (event_id) VALUES ($1) ON CONFLICT (event_id) DO NOTHING RETURNING event_id",
    )
    .bind(pg(Some(event_id)))
    .fetch_optional(pool)
    .await
    .map_err(|error| database_message(&error))?;
    Ok(claimed.is_some())
}

async fn record_slack_turn_launch(pool: &PgPool, record: &LaunchRecord<'_>) -> Result<String, String> {
    if !js_truthy(record.conversation_id) || !js_truthy(record.turn_id) || !js_truthy(record.status_message_ts) {
        return Err("Slack 업무 연결 정보가 완전하지 않습니다.".into());
    }
    let updated = sqlx::query_scalar::<_, String>(
        "UPDATE slack_threads
         SET conversation_id = $4::uuid, last_turn_id = $5::uuid, status_message_ts = $6,
             delivery_completed_at = NULL, updated_at = now()
         WHERE team_id = $1 AND channel_id = $2 AND thread_ts = $3
         RETURNING conversation_id::text",
    )
    .bind(pg(Some(record.team_id)))
    .bind(pg(Some(record.channel_id)))
    .bind(pg(Some(record.thread_ts)))
    .bind(pg(record.conversation_id))
    .bind(pg(record.turn_id))
    .bind(pg(record.status_message_ts))
    .fetch_optional(pool)
    .await
    .map_err(|error| database_message(&error))?;
    updated.ok_or_else(|| "Slack 스레드 연결 정보를 찾을 수 없습니다.".to_string())
}

async fn pending_slack_turn_targets(pool: &PgPool) -> Result<Vec<PendingTarget>, String> {
    let rows = sqlx::query(
        "SELECT channel_id, thread_ts, status_message_ts, last_turn_id::text AS turn_id
         FROM slack_threads
         WHERE last_turn_id IS NOT NULL AND status_message_ts IS NOT NULL AND delivery_completed_at IS NULL
         ORDER BY updated_at, team_id, channel_id, thread_ts",
    )
    .fetch_all(pool)
    .await
    .map_err(|error| database_message(&error))?;
    Ok(rows
        .iter()
        .map(|row| PendingTarget {
            channel_id: row.get("channel_id"),
            thread_ts: row.get("thread_ts"),
            message_ts: row.get("status_message_ts"),
            turn_id: row.get("turn_id"),
        })
        .collect())
}

async fn mark_slack_turn_delivery_completed(pool: &PgPool, turn_id: &Value) -> Result<(), String> {
    sqlx::query("UPDATE slack_threads SET delivery_completed_at = now(), updated_at = now() WHERE last_turn_id = $1::uuid")
        .bind(pg(Some(turn_id)))
        .execute(pool)
        .await
        .map_err(|error| database_message(&error))?;
    Ok(())
}

async fn slack_preference(pool: &PgPool, team_id: &Value, user_id: Option<&Value>) -> Result<Option<String>, String> {
    sqlx::query_scalar::<_, String>(
        "SELECT character_id FROM slack_user_preferences WHERE team_id = $1 AND user_id = $2 ORDER BY updated_at DESC LIMIT 1",
    )
    .bind(pg(Some(team_id)))
    .bind(pg(user_id))
    .fetch_optional(pool)
    .await
    .map_err(|error| database_message(&error))
}

async fn save_slack_preference(pool: &PgPool, team_id: &Value, user_id: Option<&Value>, character_id: Option<&Value>) -> Result<(), String> {
    sqlx::query(
        "INSERT INTO slack_user_preferences (team_id, user_id, character_id, updated_at)
         VALUES ($1, $2, $3, now())
         ON CONFLICT (team_id, user_id) DO UPDATE SET character_id = EXCLUDED.character_id, updated_at = now()",
    )
    .bind(pg(Some(team_id)))
    .bind(pg(user_id))
    .bind(pg(character_id))
    .execute(pool)
    .await
    .map_err(|error| database_message(&error))?;
    Ok(())
}

/// `backendJSON`이 던지는 오류. status는 응답이 왔을 때만 있다.
struct BackendError {
    message: String,
    status: Option<u16>,
}

/// `backendJSON`. 본문이 JSON이 아니면 {}로 보고, 2xx가 아니면 payload.error 또는 상태 코드 문구로 실패한다.
async fn backend_json(http: &reqwest::Client, method: reqwest::Method, url: &str, body: Option<Value>) -> Result<Value, BackendError> {
    let fetch_failed = || BackendError { message: "fetch failed".into(), status: None };
    let mut request = http.request(method, url);
    if let Some(body) = body {
        let bytes = crate::js::to_js_json(&body).map_err(|_| fetch_failed())?;
        request = request.header("content-type", "application/json").body(bytes);
    }
    let response = request.send().await.map_err(|_| fetch_failed())?;
    let status = response.status();
    let payload = match response.bytes().await {
        Ok(bytes) => parse_js_json(&String::from_utf8_lossy(&bytes)).unwrap_or_else(|_| json!({})),
        Err(_) => json!({}),
    };
    if !status.is_success() {
        let error = read(Some(&payload), "error").map_err(|message| BackendError { message, status: None })?;
        let message = present(error).map_or_else(|| format!("OFFICESTRA 백엔드 오류 {}", status.as_u16()), js_string);
        return Err(BackendError { message, status: Some(status.as_u16()) });
    }
    Ok(payload)
}

/// Bolt IncomingEventType.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Incoming {
    Event,
    Command,
    Options,
    Action,
    Shortcut,
    ViewAction,
}

fn in_operator_error(key: &str, value: &Value) -> String {
    format!("TypeError: Cannot use 'in' operator to search for '{key}' in {}", js_string(value))
}

/// `getTypeAndConversation`의 종류 판별. body가 undefined·null이거나 event가 객체가 아니면 Node처럼 TypeError다.
fn incoming_type(body: Option<&Value>) -> Result<Option<Incoming>, String> {
    let fields = match body {
        None => return Err("TypeError: Cannot read properties of undefined (reading 'event')".into()),
        Some(Value::Null) => return Err("TypeError: Cannot read properties of null (reading 'event')".into()),
        Some(Value::Object(fields)) => fields,
        Some(_) => return Ok(None),
    };
    if let Some(event) = fields.get("event") {
        match event {
            Value::Object(event) => {
                if let Some(Value::Null) = event.get("channel") {
                    return Err(in_operator_error("id", &Value::Null));
                }
                if let Some(item) = event.get("item") {
                    if !item.is_object() && !item.is_array() {
                        return Err(in_operator_error("channel", item));
                    }
                }
            }
            Value::Array(_) => {}
            other => return Err(in_operator_error("channel", other)),
        }
        return Ok(Some(Incoming::Event));
    }
    let is_type = |name: &str| fields.get("type").and_then(Value::as_str) == Some(name);
    // conversationId를 구할 때 channel이 null이면 null.id를 읽는다.
    let channel = || match fields.get("channel") {
        Some(Value::Null) => Err("TypeError: Cannot read properties of null (reading 'id')".to_string()),
        _ => Ok(()),
    };
    if fields.contains_key("command") {
        return Ok(Some(Incoming::Command));
    }
    if fields.contains_key("name") || is_type("block_suggestion") {
        channel()?;
        return Ok(Some(Incoming::Options));
    }
    if fields.contains_key("actions") || is_type("dialog_submission") {
        channel()?;
        return Ok(Some(Incoming::Action));
    }
    if is_type("shortcut") {
        return Ok(Some(Incoming::Shortcut));
    }
    if is_type("message_action") {
        channel()?;
        return Ok(Some(Incoming::Shortcut));
    }
    if is_type("view_submission") || is_type("view_closed") {
        return Ok(Some(Incoming::ViewAction));
    }
    Ok(None)
}

/// buildSource의 teamId·enterpriseId(None은 undefined). 팀별 클라이언트를 고를 때만 쓴다.
#[derive(Debug, PartialEq)]
struct Source {
    team_id: Option<Value>,
    enterprise_id: Option<Value>,
}

fn build_source(kind: Incoming, body: &Value) -> Result<Source, String> {
    let field = |value: Option<&Value>, key: &str| member(value, key).cloned();
    match kind {
        Incoming::Event => {
            let first = match body.get("authorizations") {
                Some(Value::Array(items)) => items.first(),
                _ => None,
            };
            if let Some(Value::Null) = first {
                return Err("TypeError: Cannot read properties of null (reading 'is_enterprise_install')".into());
            }
            // userId를 구할 때 event.user가 null이면 null.id를 읽는다.
            if let Some(Value::Null) = member(body.get("event"), "user") {
                return Err("TypeError: Cannot read properties of null (reading 'id')".into());
            }
            Ok(match first {
                Some(first) => {
                    let team = field(Some(first), "team_id");
                    let team_id = if matches!(team, Some(Value::Null)) { field(Some(body), "team_id") } else { team };
                    Source { team_id, enterprise_id: field(Some(first), "enterprise_id").filter(|id| !id.is_null()) }
                }
                None => Source { team_id: field(Some(body), "team_id"), enterprise_id: field(Some(body), "enterprise_id") },
            })
        }
        Incoming::Command => Ok(Source { team_id: field(Some(body), "team_id"), enterprise_id: field(Some(body), "enterprise_id") }),
        Incoming::Options | Incoming::Action | Incoming::Shortcut | Incoming::ViewAction => {
            let team = present(body.get("team"));
            let parse_team = || -> Result<Option<Value>, String> {
                match team {
                    Some(team) => Ok(field(Some(team), "id")),
                    None => Ok(read(body.get("user"), "team_id").map_err(type_error)?.cloned()),
                }
            };
            let team_id = if kind == Incoming::ViewAction {
                match read(body.get("view"), "app_installed_team_id").map_err(type_error)?.filter(|id| js_truthy(Some(id))) {
                    Some(id) => Some(id.clone()),
                    None => parse_team()?,
                }
            } else {
                parse_team()?
            };
            let enterprise_id = match present(body.get("enterprise")) {
                Some(enterprise) => field(Some(enterprise), "id"),
                None => team.and_then(|team| field(Some(team), "enterprise_id")),
            };
            read(body.get("user"), "id").map_err(type_error)?;
            Ok(Source { team_id, enterprise_id })
        }
    }
}

/// auth.test 결과(context.botUserId·botId). None은 undefined다.
#[derive(Clone, Debug)]
struct AuthResult {
    bot_user_id: Option<Value>,
    bot_id: Option<Value>,
}

type AuthFuture = Shared<BoxFuture<'static, Result<AuthResult, SlackError>>>;

/// Bolt ignoreSelf. 자기 봇 메시지와 자기 사용자 ID 이벤트를 거른다(채널 입장·퇴장은 남긴다).
fn ignored_self(auth: Option<&AuthResult>, event: &Value) -> bool {
    let bot_id = auth.and_then(|auth| auth.bot_id.as_ref());
    let bot_user_id = auth.and_then(|auth| auth.bot_user_id.as_ref());
    let event_type = member(Some(event), "type").and_then(Value::as_str);
    if event_type == Some("message")
        && member(Some(event), "subtype").and_then(Value::as_str) == Some("bot_message")
        && strict_equal(bot_id, member(Some(event), "bot_id"))
    {
        return true;
    }
    let has_user = event.as_object().is_some_and(|fields| fields.contains_key("user"));
    bot_user_id.is_some()
        && has_user
        && strict_equal(member(Some(event), "user"), bot_user_id)
        && !matches!(event_type, Some("member_joined_channel" | "member_left_channel"))
}

/// 지켜보는 업무 하나(`monitorSlackTurn` target).
#[derive(Clone)]
struct MonitorTarget {
    client: WebClient,
    channel_id: Option<Value>,
    thread_ts: Option<Value>,
    message_ts: Option<Value>,
    turn_id: Value,
}

struct Bridge {
    pool: PgPool,
    backend_url: String,
    backend: reqwest::Client,
    configuration: SlackConfiguration,
    slack_http: reqwest::Client,
    api_url: String,
    timings: SlackTimings,
    app_client: WebClient,
    auth: AuthFuture,
    clients: Mutex<HashMap<String, WebClient>>,
    monitors: Mutex<HashSet<String>>,
}

impl Bridge {
    fn report_error(&self, message: &str) {
        eprintln!("Slack 연결 오류 {message}");
    }

    async fn backend(&self, method: reqwest::Method, path: &str, body: Option<Value>) -> Result<Value, BackendError> {
        backend_json(&self.backend, method, &format!("{}{path}", self.backend_url), body).await
    }

    /// processEvent의 client 선택. 봇 토큰이 있으면 팀(없으면 엔터프라이즈)별 묶음 클라이언트, 아니면 app.client다.
    fn client_for(&self, has_token: bool, source: &Source) -> WebClient {
        if !has_token {
            return self.app_client.clone();
        }
        let (key, team_id) = match (&source.team_id, &source.enterprise_id) {
            (Some(team), _) => (js_string(team), (!team.is_null()).then(|| js_string(team))),
            (None, Some(enterprise)) => (js_string(enterprise), None),
            (None, None) => return self.app_client.clone(),
        };
        let mut clients = self.clients.lock().unwrap();
        clients
            .entry(key)
            .or_insert_with(|| {
                WebClient::new(
                    self.slack_http.clone(),
                    &self.api_url,
                    &self.configuration.bot_token,
                    team_id,
                    SOCKET_RETRIES,
                    self.timings.retry_min_timeout,
                )
            })
            .clone()
    }

    /// App.processEvent. Err는 SocketModeReceiver까지 올라가는 오류이고, 처리기 오류는 app.error로 기록한다.
    async fn process_event(self: &Arc<Self>, envelope: &Envelope, acker: &Acker) -> Result<(), String> {
        let envelope_id = envelope.envelope_id.as_ref();
        let Some(kind) = incoming_type(envelope.body.as_ref())? else {
            eprintln!("[WARN]  bolt-app Could not determine the type of an incoming event. No listeners will be called.");
            return Ok(());
        };
        let Some(body) = envelope.body.as_ref() else { return Ok(()) };
        let source = build_source(kind, body)?;
        let event_type = member(body.get("event"), "type").and_then(Value::as_str);
        let skip_authorize = kind == Incoming::Event && matches!(event_type, Some("app_uninstalled" | "tokens_revoked"));
        let auth = if skip_authorize {
            None
        } else {
            match self.auth.clone().await {
                Ok(auth) => Some(auth),
                Err(error) => {
                    eprintln!("[WARN]  bolt-app Authorization of incoming event did not succeed. No listeners will be called.");
                    self.report_error(&format!("Authorization of incoming event did not succeed. {}", error.message()));
                    return acker.ack(envelope_id).map_err(|message| format!("Error: {message}"));
                }
            }
        };
        let client = self.client_for(auth.is_some(), &source);
        let result = match kind {
            Incoming::Event => {
                let event = body.get("event").unwrap_or(&Value::Null);
                // 이벤트는 처리기 전에 바로 ack한다(function_executed만 처리기가 ack한다).
                if event_type != Some("function_executed") {
                    acker.ack(envelope_id).map_err(|message| format!("Error: {message}"))?;
                }
                if ignored_self(auth.as_ref(), event) {
                    return Ok(());
                }
                match event_type {
                    Some("app_mention") => self.receive_slack_message(body, event, &client, auth.as_ref()).await,
                    Some("message") if member(Some(event), "channel_type").and_then(Value::as_str) == Some("im") => {
                        self.receive_slack_message(body, event, &client, auth.as_ref()).await
                    }
                    _ => Ok(()),
                }
            }
            Incoming::Command if member(Some(body), "command").and_then(Value::as_str) == Some("/office") => {
                self.office_command(body, &client, acker, envelope_id).await
            }
            Incoming::Action => {
                let payload = match body.get("actions") {
                    Some(Value::Array(actions)) => actions.first(),
                    Some(_) => None,
                    None => Some(body),
                };
                let matched = payload
                    .and_then(|payload| member(Some(payload), "action_id"))
                    .and_then(Value::as_str)
                    .is_some_and(|action_id| action_id.starts_with(SELECT_ACTION_PREFIX));
                match payload {
                    Some(action) if matched => self.select_character(body, action, &client, acker, envelope_id).await,
                    _ => Ok(()),
                }
            }
            _ => Ok(()),
        };
        if let Err(message) = result {
            self.report_error(&message);
        }
        Ok(())
    }

    async fn list_characters(&self, method: &str) -> Result<Vec<Value>, String> {
        let payload = self.backend(reqwest::Method::GET, "/api/characters", None).await.map_err(|error| error.message)?;
        match read(Some(&payload), "characters")? {
            None | Some(Value::Null) => Ok(Vec::new()),
            Some(Value::Array(characters)) => Ok(characters.clone()),
            Some(_) => Err(format!("characters.{method} is not a function")),
        }
    }

    /// `characterByID`. 찾은 값이 없으면 None이다.
    async fn character_by_id(&self, id: Option<&Value>) -> Result<Option<Value>, String> {
        for character in self.list_characters("find").await? {
            if strict_equal(read(Some(&character), "id")?, id) {
                return Ok(Some(character));
            }
        }
        Ok(None)
    }

    /// `employeeSelectionMessage`. 직원 목록과 저장된 선택을 함께 읽는다.
    async fn employee_selection_message(&self, team_id: &Value, user_id: Option<&Value>) -> Result<Vec<(&'static str, Option<Value>)>, String> {
        let (characters, preference) =
            tokio::try_join!(self.list_characters("slice"), slack_preference(&self.pool, team_id, user_id))?;
        let selected = preference.map(Value::String);
        Ok(vec![
            ("text", Some(json!("OFFICESTRA 직원을 선택하세요."))),
            ("blocks", Some(employee_selection_blocks(&characters, selected.as_ref()))),
        ])
    }

    async fn find_or_create_slack_thread(&self, team_id: &Value, channel_id: &Value, thread_ts: &Value, user_id: &Value) -> Result<SlackThread, String> {
        let thread = |row: &sqlx::postgres::PgRow| SlackThread { character_id: row.get("character_id"), conversation_id: row.get("conversation_id") };
        let existing = sqlx::query(
            "SELECT character_id, conversation_id::text AS conversation_id FROM slack_threads
             WHERE team_id = $1 AND channel_id = $2 AND thread_ts = $3",
        )
        .bind(pg(Some(team_id)))
        .bind(pg(Some(channel_id)))
        .bind(pg(Some(thread_ts)))
        .fetch_optional(&self.pool)
        .await
        .map_err(|error| database_message(&error))?;
        if let Some(row) = existing {
            return Ok(thread(&row));
        }
        let preference = slack_preference(&self.pool, team_id, Some(user_id)).await?;
        let characters = self.list_characters("find").await?;
        let character_id = match preference {
            Some(id) => Some(Value::String(id)),
            None => {
                let mut boss = None;
                for character in &characters {
                    if strict_equal(read(Some(character), "id")?, Some(&json!("boss"))) {
                        boss = Some(character);
                        break;
                    }
                }
                present(member(boss, "id")).or_else(|| present(member(characters.first(), "id"))).cloned()
            }
        };
        let Some(character_id) = character_id.filter(|id| js_truthy(Some(id))) else {
            return Err("등록된 직원이 없습니다.".into());
        };
        let inserted = sqlx::query(
            "INSERT INTO slack_threads (team_id, channel_id, thread_ts, user_id, character_id)
             VALUES ($1, $2, $3, $4, $5)
             ON CONFLICT (team_id, channel_id, thread_ts) DO UPDATE SET updated_at = now()
             RETURNING character_id, conversation_id::text AS conversation_id",
        )
        .bind(pg(Some(team_id)))
        .bind(pg(Some(channel_id)))
        .bind(pg(Some(thread_ts)))
        .bind(pg(Some(user_id)))
        .bind(pg(Some(&character_id)))
        .fetch_one(&self.pool)
        .await
        .map_err(|error| database_message(&error))?;
        Ok(thread(&inserted))
    }

    /// `/office` 명령. 먼저 ack하고 권한이 없으면 안내만 보낸다.
    async fn office_command(&self, body: &Value, client: &WebClient, acker: &Acker, envelope_id: Option<&Value>) -> Result<(), String> {
        acker.ack(envelope_id)?;
        let channel = member(Some(body), "channel_id").cloned();
        if !is_authorized_slack_request(&self.configuration, body) {
            call(client, "chat.postMessage", arguments(vec![("channel", channel), ("text", Some(json!("OFFICESTRA 사용 권한이 없습니다.")))])).await?;
            return Ok(());
        }
        let mut entries = vec![("channel", channel)];
        entries.extend(self.employee_selection_message(&slack_team_id(body), member(Some(body), "user_id")).await?);
        call(client, "chat.postMessage", arguments(entries)).await?;
        Ok(())
    }

    /// officestra.select-character.* 버튼. 선택을 저장하고 원래 메시지를 바꾼다.
    async fn select_character(&self, body: &Value, action: &Value, client: &WebClient, acker: &Acker, envelope_id: Option<&Value>) -> Result<(), String> {
        acker.ack(envelope_id)?;
        if !is_authorized_slack_request(&self.configuration, body) {
            return Ok(());
        }
        let character = self.character_by_id(member(Some(action), "value")).await?;
        let channel = || read(read(Some(body), "channel")?, "id").map(|id| id.cloned());
        let message_ts = || read(read(Some(body), "message")?, "ts").map(|ts| ts.cloned());
        let Some(character) = character.filter(|character| js_truthy(Some(character))) else {
            let text = Some(json!("직원 정보를 다시 불러온 뒤 선택해 주세요."));
            call(client, "chat.update", arguments(vec![("channel", channel()?), ("ts", message_ts()?), ("text", text)])).await?;
            return Ok(());
        };
        let team_id = slack_team_id(body);
        let user_id = read(read(Some(body), "user")?, "id")?;
        save_slack_preference(&self.pool, &team_id, user_id, read(Some(&character), "id")?).await?;
        let text = format!(
            "✅ 기본 직원을 *{}*으로 선택했습니다. 이제 봇에게 업무를 보내세요.",
            escape_value(member(Some(&character), "name"))
        );
        call(client, "chat.update", arguments(vec![("channel", channel()?), ("ts", message_ts()?), ("text", Some(json!(text)))])).await?;
        Ok(())
    }

    /// `receiveSlackMessage`. 허용 사용자의 사람 메시지만 받아 직원 업무를 시작하고 결과를 지켜본다.
    async fn receive_slack_message(self: &Arc<Self>, body: &Value, event: &Value, client: &WebClient, auth: Option<&AuthResult>) -> Result<(), String> {
        let get = |key: &str| member(Some(event), key);
        if !js_truthy(Some(event))
            || js_truthy(get("bot_id"))
            || js_truthy(get("subtype"))
            || !js_truthy(get("user"))
            || !js_truthy(get("channel"))
            || !js_truthy(get("ts"))
            || !is_authorized_slack_request(&self.configuration, body)
        {
            return Ok(());
        }
        if let Some(event_id) = member(Some(body), "event_id").filter(|id| js_truthy(Some(id))) {
            if !claim_slack_event(&self.pool, event_id).await? {
                return Ok(());
            }
        }
        let team_id = slack_team_id(body);
        let prompt = strip_slack_mention(get("text"), auth.and_then(|auth| auth.bot_user_id.as_ref()));
        let channel = get("channel").cloned().unwrap_or(Value::Null);
        let thread_ts = present(get("thread_ts")).or(get("ts")).cloned().unwrap_or(Value::Null);
        if prompt.is_empty() {
            let mut entries = vec![("channel", Some(channel)), ("thread_ts", Some(thread_ts))];
            entries.extend(self.employee_selection_message(&team_id, get("user")).await?);
            call(client, "chat.postMessage", arguments(entries)).await?;
            return Ok(());
        }
        let user = get("user").cloned().unwrap_or(Value::Null);
        let thread = self.find_or_create_slack_thread(&team_id, &channel, &thread_ts, &user).await?;
        let character = self.character_by_id(Some(&Value::String(thread.character_id.clone()))).await?;
        let Some(character) = character.filter(|character| js_truthy(Some(character))) else {
            let text = json!("선택된 직원을 찾을 수 없습니다. `/office`에서 다시 선택해 주세요.");
            call(client, "chat.postMessage", arguments(vec![("channel", Some(channel)), ("thread_ts", Some(thread_ts)), ("text", Some(text))])).await?;
            return Ok(());
        };
        let name = member(Some(&character), "name").cloned();
        let starting = json!({ "characterName": name.clone(), "status": "starting" });
        let status_message = call(
            client,
            "chat.postMessage",
            arguments(vec![
                ("channel", Some(channel.clone())),
                ("thread_ts", Some(thread_ts.clone())),
                ("text", Some(json!(format!("⏳ {} 업무를 시작합니다.", template(name.as_ref()))))),
                ("blocks", Some(progress_blocks(&starting))),
            ]),
        )
        .await?;
        let message_ts = member(Some(&status_message), "ts").cloned();
        let launched: Result<(), String> = async {
            let mut request = Map::new();
            if let Some(id) = member(Some(&character), "id") {
                request.insert("characterId".into(), id.clone());
            }
            request.insert("prompt".into(), json!(prompt));
            request.insert("conversationId".into(), Value::String(thread.conversation_id.clone()));
            let job = self
                .backend(reqwest::Method::POST, "/api/agent-jobs", Some(Value::Object(request)))
                .await
                .map_err(|error| error.message)?;
            let conversation_id = read(Some(&job), "conversationId")?;
            let turn_id = read(Some(&job), "turnId")?;
            let record = LaunchRecord {
                team_id: &team_id,
                channel_id: &channel,
                thread_ts: &thread_ts,
                conversation_id,
                turn_id,
                status_message_ts: message_ts.as_ref(),
            };
            record_slack_turn_launch(&self.pool, &record).await?;
            self.monitor_slack_turn(MonitorTarget {
                client: client.clone(),
                channel_id: Some(channel.clone()),
                thread_ts: Some(thread_ts.clone()),
                message_ts: message_ts.clone(),
                turn_id: turn_id.cloned().unwrap_or(Value::Null),
            });
            Ok(())
        }
        .await;
        if let Err(message) = launched {
            let text = format!("❌ {} 업무를 시작하지 못했습니다. {message}", template(name.as_ref()));
            call(client, "chat.update", arguments(vec![("channel", Some(channel)), ("ts", message_ts), ("text", Some(json!(text)))])).await?;
        }
        Ok(())
    }

    /// `monitorSlackTurn`. 같은 업무를 두 번 지켜보지 않고, 끝나면 목록에서 뺀다.
    fn monitor_slack_turn(self: &Arc<Self>, target: MonitorTarget) {
        let key = target.turn_id.to_string();
        if !self.monitors.lock().unwrap().insert(key.clone()) {
            return;
        }
        let bridge = Arc::clone(self);
        tokio::spawn(async move {
            if let Err(message) = bridge.run_turn_monitor(&target).await {
                eprintln!("Slack 업무 모니터 오류 {message}");
            }
            bridge.monitors.lock().unwrap().remove(&key);
        });
    }

    /// `runTurnMonitor`. 1.5초마다 실시간 기록을 읽어 상태 메시지를 바꾸고, 끝나면 응답을 스레드에 나눠 보낸다.
    async fn run_turn_monitor(&self, target: &MonitorTarget) -> Result<(), String> {
        let path = format!("/api/live-feed/{}", encode_uri_component(&template(Some(&target.turn_id))));
        let mut previous = String::new();
        loop {
            let payload = match self.backend(reqwest::Method::GET, &path, None).await {
                Ok(payload) => payload,
                Err(error) if error.status == Some(404) => {
                    tokio::time::sleep(self.timings.poll_interval).await;
                    continue;
                }
                Err(error) => return Err(error.message),
            };
            let turn = match read(Some(&payload), "turn")? {
                None => return Err("Cannot read properties of undefined (reading 'characterName')".into()),
                Some(Value::Null) => return Err("Cannot read properties of null (reading 'characterName')".into()),
                Some(turn) => turn.clone(),
            };
            let text = render_slack_turn(&turn);
            if text != previous {
                let entries = vec![
                    ("channel", target.channel_id.clone()),
                    ("ts", target.message_ts.clone()),
                    ("text", Some(json!(text))),
                    ("blocks", Some(progress_blocks(&turn))),
                ];
                call(&target.client, "chat.update", arguments(entries)).await?;
                previous = text;
            }
            if is_terminal(&turn) {
                if let Some(response) = trimmed(member(Some(&turn), "response")).filter(|response| !response.is_empty()) {
                    for chunk in split_slack_message(response, MESSAGE_TEXT_LIMIT) {
                        let entries = vec![
                            ("channel", target.channel_id.clone()),
                            ("thread_ts", target.thread_ts.clone()),
                            ("text", Some(json!(normalize_slack_markdown(&chunk)))),
                        ];
                        call(&target.client, "chat.postMessage", arguments(entries)).await?;
                    }
                }
                mark_slack_turn_delivery_completed(&self.pool, &target.turn_id).await?;
                return Ok(());
            }
            tokio::time::sleep(self.timings.poll_interval).await;
        }
    }

    /// `restoreSlackTurnMonitors`. 배달을 마치지 못한 업무를 app.client로 다시 지켜본다.
    async fn restore_slack_turn_monitors(self: &Arc<Self>) -> Result<usize, String> {
        let targets = pending_slack_turn_targets(&self.pool).await?;
        for target in &targets {
            self.monitor_slack_turn(MonitorTarget {
                client: self.app_client.clone(),
                channel_id: Some(json!(target.channel_id)),
                thread_ts: Some(json!(target.thread_ts)),
                message_ts: Some(json!(target.message_ts)),
                turn_id: json!(target.turn_id),
            });
        }
        Ok(targets.len())
    }
}

/// 시간 설정. 기본값은 Node 판과 같고 시험에서만 줄인다.
#[derive(Clone, Copy, Debug)]
pub struct SlackTimings {
    pub poll_interval: Duration,
    pub socket: SocketTimings,
    /// Web API 재시도 최소 대기(p-retry minTimeout).
    pub retry_min_timeout: Duration,
}

impl Default for SlackTimings {
    fn default() -> Self {
        Self {
            poll_interval: Duration::from_millis(1_500),
            socket: SocketTimings::default(),
            retry_min_timeout: Duration::from_millis(1_000),
        }
    }
}

pub struct SlackBridgeOptions {
    pub pool: PgPool,
    /// 끝의 / 하나는 뗀다.
    pub backend_url: String,
    pub environment: Vec<(String, String)>,
    /// None이면 OFFICE_SLACK_ENV_FILE, 그다음 ~/.officestra/slack.env다.
    pub file_path: Option<String>,
    pub api_url: String,
    pub timings: SlackTimings,
}

impl SlackBridgeOptions {
    /// 운영 기본값. 환경은 지금 프로세스의 값이다.
    pub fn new(pool: PgPool, backend_url: String) -> Self {
        let environment = std::env::vars_os()
            .map(|(key, value)| (key.to_string_lossy().into_owned(), value.to_string_lossy().into_owned()))
            .collect();
        Self { pool, backend_url, environment, file_path: None, api_url: SLACK_API_URL.into(), timings: SlackTimings::default() }
    }
}

/// 연결된 Slack 앱. drop하면 소켓 연결을 멈춘다.
pub struct SlackBridge {
    socket: SocketMode,
    bridge: Arc<Bridge>,
}

impl SlackBridge {
    pub fn stop(&self) {
        self.socket.stop();
    }

    /// 지금 지켜보는 업무 수.
    pub fn active_monitors(&self) -> usize {
        self.bridge.monitors.lock().unwrap().len()
    }
}

pub enum SlackBridgeStart {
    Disabled(&'static str),
    Enabled(SlackBridge),
}

/// 시작 실패. 소켓이 이미 열린 뒤의 실패이면 Node처럼 연결은 계속 살아 있다.
pub struct SlackStartFailure {
    pub message: String,
    pub running: Option<SlackBridge>,
}

/// `startSlackBridge`. 토큰·허용 사용자가 없으면 끄고, 있으면 auth.test를 바로 시작한 뒤 소켓 연결·모니터 복구를 한다.
pub async fn start_slack_bridge(options: SlackBridgeOptions) -> Result<SlackBridgeStart, SlackStartFailure> {
    let failure = |message: String| SlackStartFailure { message, running: None };
    let configuration = read_slack_configuration(&options.environment, options.file_path.as_deref()).map_err(failure)?;
    if configuration.bot_token.is_empty() || configuration.app_token.is_empty() {
        return Ok(SlackBridgeStart::Disabled("missing_tokens"));
    }
    if configuration.allowed_user_ids.is_empty() {
        eprintln!("Slack 연동 비활성화. OFFICE_SLACK_ALLOWED_USER_IDS가 필요합니다.");
        return Ok(SlackBridgeStart::Disabled("missing_allowed_users"));
    }
    let build = |builder: reqwest::ClientBuilder| builder.no_proxy().build().map_err(|error| failure(error.to_string()));
    // Node fetch(redirect:'error')처럼 Slack 응답의 3xx를 따라가지 않는다.
    let slack_http = build(crate::http_client_builder().redirect(reqwest::redirect::Policy::none()))?;
    let backend = build(crate::http_client_builder())?;
    let timings = options.timings;
    let app_client = WebClient::new(slack_http.clone(), &options.api_url, &configuration.bot_token, None, TEN_RETRIES, timings.retry_min_timeout);
    let auth_client = app_client.clone();
    let bot_token = configuration.bot_token.clone();
    let auth_task = tokio::spawn(async move {
        let result = auth_client.api_call("auth.test", json!({ "token": bot_token })).await?;
        Ok::<_, SlackError>(AuthResult { bot_user_id: result.get("user_id").cloned(), bot_id: result.get("bot_id").cloned() })
    });
    let auth: AuthFuture = async move {
        auth_task.await.unwrap_or_else(|_| Err(SlackError::Plain("auth.test 작업이 중단되었습니다.".into())))
    }
    .boxed()
    .shared();
    // Node는 이 실패를 처리하지 않아 프로세스가 죽는다. 여기서는 기록만 하고 이벤트마다 인증 실패로 처리한다.
    let watched = auth.clone();
    tokio::spawn(async move {
        if let Err(error) = watched.await {
            eprintln!("Slack 연결 오류 {}", error.message());
        }
    });
    let socket_client = WebClient::new(slack_http.clone(), &options.api_url, &configuration.app_token, None, SOCKET_RETRIES, timings.retry_min_timeout);
    let bridge = Arc::new(Bridge {
        pool: options.pool,
        backend_url: options.backend_url.strip_suffix('/').unwrap_or(&options.backend_url).to_string(),
        backend,
        configuration,
        slack_http,
        api_url: options.api_url,
        timings,
        app_client,
        auth,
        clients: Mutex::new(HashMap::new()),
        monitors: Mutex::new(HashSet::new()),
    });
    let handler: socket::EnvelopeHandler = {
        let bridge = Arc::clone(&bridge);
        Arc::new(move |envelope: Envelope, acker: Acker| {
            let bridge = Arc::clone(&bridge);
            tokio::spawn(async move {
                if let Err(error) = bridge.process_event(&envelope, &acker).await {
                    // SocketModeReceiver의 기본 오류 처리. ack하지 않는다.
                    eprintln!(
                        "[ERROR]  bolt-app An unhandled error occurred while Bolt processed (type: {}, error: {error})",
                        template(member(envelope.body.as_ref(), "type"))
                    );
                }
            });
        })
    };
    let socket = SocketMode::start(socket_client, timings.socket, handler).await.map_err(|error| failure(error.message()))?;
    let running = SlackBridge { socket, bridge: Arc::clone(&bridge) };
    match bridge.restore_slack_turn_monitors().await {
        Ok(count) => {
            println!("OFFICESTRA Slack Socket Mode 연결 완료");
            if count > 0 {
                println!("Slack 업무 모니터 {count}건 복구 완료");
            }
            Ok(SlackBridgeStart::Enabled(running))
        }
        Err(message) => Err(SlackStartFailure { message, running: Some(running) }),
    }
}

/// server.mjs의 `void startSlackBridge(...).catch(...)`. 연결된 동안에는 끝나지 않는다.
pub async fn serve_slack_bridge(options: SlackBridgeOptions) {
    let running = match start_slack_bridge(options).await {
        Ok(SlackBridgeStart::Enabled(running)) => running,
        Ok(SlackBridgeStart::Disabled(_)) => return,
        Err(failure) => {
            eprintln!("Slack 연동을 시작하지 못했습니다. {}", failure.message);
            match failure.running {
                Some(running) => running,
                None => return,
            }
        }
    };
    std::future::pending::<()>().await;
    drop(running);
}
