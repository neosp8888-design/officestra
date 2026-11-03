// 공급자 계정 한도와 로컬 DB 사용 통계를 합친다. Node usage-summary.mjs를 옮겼다.

use std::collections::BTreeMap;
use std::ffi::{CString, OsString};
use std::future::Future;
use std::io;
use std::os::unix::ffi::OsStringExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, LazyLock, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use futures_util::FutureExt;
use futures_util::future::{BoxFuture, Shared};
use regex::Regex;
use serde::Serialize;
use serde_json::{Map, Value, json};
use sqlx::PgPool;
use tokio::io::AsyncWriteExt;
use tokio::process::{Child, ChildStderr};
use tokio::time::Instant;

use crate::antigravity_playwright::antigravity_playwright_environment;
use crate::child_process::{DEFAULT_MAX_BUFFER, exec_file, read_open_pipe};
use crate::config::{Config, Secret};
use crate::js::{
    date_parse, is_js_whitespace, iso_string, js_length, js_max, js_min, js_string, js_string_or_empty, js_to_number,
    js_trim, js_truthy, node_tmpdir, now_milliseconds, random_uuid, uv_error,
};
use crate::local_artifacts::home_directory;
use crate::rag::database_message;

const FIVE_HOUR_MINUTES: f64 = 300.0;
const SEVEN_DAY_MINUTES: f64 = 10_080.0;
const FIVE_HOUR_KEYS: [&str; 3] = ["five-hour", "five_hour", "5h"];
pub const CLAUDE_USAGE_URL: &str = "https://api.anthropic.com/api/oauth/usage";

/// 정규식 문자 집합 안에 넣는 JS `\s`(WhiteSpace와 LineTerminator). Rust `\s`와 범위가 달라 직접 적는다.
const JS_SPACE: &str = r"\t\n\x0B\x0C\r \x{A0}\x{1680}\x{2000}-\x{200A}\x{2028}\x{2029}\x{202F}\x{205F}\x{3000}\x{FEFF}";

fn pattern(source: &str) -> Regex {
    Regex::new(source).expect("사용량 요약 정규식")
}

static CLAUDE_MAX_5X: LazyLock<Regex> = LazyLock::new(|| pattern(&format!(r"max[{JS_SPACE}_\-]*5x")));
static CLAUDE_MAX_20X: LazyLock<Regex> = LazyLock::new(|| pattern(&format!(r"max[{JS_SPACE}_\-]*20x")));
// JS 정규식의 i 깃발(유니코드 없이)은 ASCII 글자만 대소문자를 겹치므로 (?i-u:…)로 낱말에만 건다.
static ARGON_MODEL: LazyLock<Regex> = LazyLock::new(|| pattern(r"(?:^|[-_])(?i-u:argon)(?:[-_]|$)"));
static THIRD_PARTY_MODEL: LazyLock<Regex> = LazyLock::new(|| pattern(r"^(?i-u:claude|gpt|o[0-9])[-_]"));
// JS `.`은 줄 끝 문자 넷(\n·\r·U+2028·U+2029)만 빼고 맞는다.
static ARGON_GROUP_NAME: LazyLock<Regex> =
    LazyLock::new(|| pattern(r"(?i-u:gemini)[^\n\r\x{2028}\x{2029}]*(?i-u:argon)"));
static LANGUAGE_SERVER_PORT: LazyLock<Regex> =
    LazyLock::new(|| pattern(&format!(r"random port at ([0-9]+) for HTTP(?:[{JS_SPACE}]|$)")));

/// 한 한도 창의 남은 비율(0~100)과 초기화 시각.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LimitWindow {
    pub remaining: Option<i64>,
    pub reset_at: Option<String>,
}

/// Node readCodexRateLimits의 결과. parseCodexRateLimits만 거친 값은 구독 만료가 비어 있다.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CodexLimits {
    pub five_hour: Option<LimitWindow>,
    pub weekly: Option<LimitWindow>,
    pub plan: Option<String>,
    pub subscription_expires_at: Option<String>,
}

/// Claude 모델별 주간 한도(weekly_scoped) 한 줄.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelWeeklyLimit {
    pub id: String,
    pub name: String,
    pub remaining: Option<i64>,
    pub reset_at: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ClaudeLimits {
    pub five_hour: Option<LimitWindow>,
    pub weekly: Option<LimitWindow>,
    pub model_weekly_limits: Vec<ModelWeeklyLimit>,
    pub plan: Option<String>,
}

/// Antigravity 한도 그룹 안의 버킷 하나.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct QuotaBucket {
    pub id: String,
    pub name: String,
    pub window: String,
    pub remaining: Option<i64>,
    pub reset_at: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct QuotaGroup {
    pub id: String,
    pub name: String,
    pub buckets: Vec<QuotaBucket>,
}

/// Node readAntigravityRateLimits의 결과. parseAntigravityRateLimits만 거친 값은 이미지 쿨다운이 비어 있다.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AntigravityLimits {
    pub five_hour: Option<LimitWindow>,
    pub weekly: Option<LimitWindow>,
    pub plan: Option<String>,
    pub groups: Vec<QuotaGroup>,
    pub image_reset_at: Option<String>,
}

/// JS `value?.key`. 객체가 아니면 undefined다.
fn field<'a>(value: Option<&'a Value>, key: &str) -> Option<&'a Value> {
    value?.as_object()?.get(key)
}

/// `??` 판정용. undefined와 null을 없는 값으로 본다.
fn present(value: Option<&Value>) -> Option<&Value> {
    value.filter(|value| !value.is_null())
}

/// Node finiteNumber. 없는 값(undefined)은 NaN이라 None, null은 0이다.
fn finite_number(value: Option<&Value>) -> Option<f64> {
    value.map(js_to_number).filter(|number| number.is_finite())
}

/// Math.round. 소수부가 정확히 .5면 +∞ 쪽으로 올린다.
pub fn js_round(value: f64) -> f64 {
    let floor = value.floor();
    if value - floor >= 0.5 { floor + 1.0 } else { floor }
}

fn remaining_percent(used: Option<&Value>) -> Option<i64> {
    let used = finite_number(used)?;
    Some(js_round(js_min(100.0, js_max(0.0, 100.0 - used))) as i64)
}

fn remaining_fraction(fraction: f64) -> i64 {
    js_round(js_min(1.0, js_max(0.0, fraction)) * 100.0) as i64
}

/// JS TimeClip. NaN이나 ±8.64e15 밖은 Invalid Date다.
fn time_clip(time: f64) -> Option<f64> {
    (time.is_finite() && time.abs() <= 8.64e15).then(|| time.trunc() + 0.0)
}

/// JS `new Date(value).getTime()`. 숫자는 number_scale을 곱하고, 문자열·배열은 Date.parse,
/// 참거짓은 0·1밀리초, null은 0, 객체는 NaN이다.
fn date_time(value: &Value, number_scale: f64) -> Option<f64> {
    let time = match value {
        Value::Number(number) => number.as_f64()? * number_scale,
        Value::Bool(flag) => f64::from(u8::from(*flag)),
        Value::Null => 0.0,
        Value::String(_) | Value::Array(_) => date_parse(&js_string(value))?,
        Value::Object(_) => return None,
    };
    time_clip(time)
}

/// Node isoDate. null·undefined는 null이고 숫자는 초 단위다.
fn iso_date(value: Option<&Value>) -> Option<String> {
    date_time(present(value)?, 1_000.0).map(iso_string)
}

/// Node normalizedPlan. 낱말 첫 UTF-16 단위만 대문자로 바꾸므로 보조 평면 글자로 시작하면 그대로다.
fn normalized_plan(value: Option<&Value>) -> Option<String> {
    let text = js_string_or_empty(value);
    let plan = js_trim(&text);
    if plan.is_empty() {
        return None;
    }
    let words: Vec<String> = plan
        .split(|character: char| character == '-' || character == '_' || is_js_whitespace(character))
        .filter(|word| !word.is_empty())
        .map(capitalized)
        .collect();
    Some(words.join(" "))
}

fn capitalized(word: &str) -> String {
    let mut characters = word.chars();
    let Some(first) = characters.next() else {
        return String::new();
    };
    if u32::from(first) > 0xFFFF {
        return word.to_string();
    }
    first.to_uppercase().chain(characters).collect()
}

fn normalized_codex_plan(value: Option<&Value>) -> Option<String> {
    let text = js_string_or_empty(value);
    let key: String = js_trim(&text)
        .to_lowercase()
        .chars()
        .filter(|character| !(is_js_whitespace(*character) || *character == '_' || *character == '-'))
        .collect();
    match key.as_str() {
        "pro" => Some("Pro 20x".into()),
        "prolite" => Some("Pro 5x".into()),
        _ => normalized_plan(value),
    }
}

/// rateLimitTier는 default_claude_max_5x처럼 배수를 담을 때만 쓴다. 배수가 없으면 구독 종류를 다듬는다.
fn normalized_claude_plan(tier: Option<&Value>, subscription_type: Option<&Value>) -> Option<String> {
    let key = js_trim(&js_string_or_empty(tier)).to_lowercase();
    if CLAUDE_MAX_5X.is_match(&key) {
        return Some("Max 5x".into());
    }
    if CLAUDE_MAX_20X.is_match(&key) {
        return Some("Max 20x".into());
    }
    normalized_plan(subscription_type)
}

fn codex_window(window: &Value, expected_minutes: f64) -> Option<LimitWindow> {
    if finite_number(field(Some(window), "windowDurationMins")) != Some(expected_minutes) {
        return None;
    }
    Some(LimitWindow {
        remaining: remaining_percent(field(Some(window), "usedPercent")),
        reset_at: finite_number(field(Some(window), "resetsAt"))
            .and_then(|seconds| time_clip(seconds * 1_000.0))
            .map(iso_string),
    })
}

/// Node parseCodexRateLimits. `result?.rateLimits ?? result ?? {}`의 primary·secondary를 창 길이로 가른다.
pub fn parse_codex_rate_limits(result: Option<&Value>) -> CodexLimits {
    let empty = Value::Object(Map::new());
    let rate_limits = present(field(result, "rateLimits")).or(present(result)).unwrap_or(&empty);
    let windows: Vec<&Value> = ["primary", "secondary"]
        .iter()
        .filter_map(|key| field(Some(rate_limits), key))
        .filter(|window| js_truthy(Some(window)))
        .collect();
    let window = |minutes: f64| windows.iter().find_map(|entry| codex_window(entry, minutes));
    CodexLimits {
        five_hour: window(FIVE_HOUR_MINUTES),
        weekly: window(SEVEN_DAY_MINUTES),
        plan: normalized_codex_plan(field(Some(rate_limits), "planType")),
        subscription_expires_at: None,
    }
}

/// Node `Buffer.from(text, "base64url")`. Node 24의 레거시 nbytes 해독기를 그대로 옮겼다.
/// JS 문자열의 UTF-16 단위는 아래 바이트만 쓰고, 끝의 '=' 판정만 단위 전체로 한다.
/// 잘못된 글자는 건너뛰고, 중간의 '='에서 멈춘다.
pub fn base64url_decode(text: &str) -> Vec<u8> {
    let units: Vec<u16> = text.encode_utf16().collect();
    let source_length = units.len();
    let padding = |index: usize| units[index] == 0x3d;
    let mut significant = source_length;
    if significant > 0 && padding(significant - 1) {
        significant -= 1;
    }
    if significant > 1 && padding(significant - 1) {
        significant -= 1;
    }
    let destination_length = (significant * 3) >> 2;
    let mut size = source_length;
    let mut decoded_size = 0;
    if size >= 2 {
        if padding(size - 1) {
            size -= 1;
            if padding(size - 1) {
                size -= 1;
            }
        }
        decoded_size = if size > 1 { size / 4 * 3 + (size % 4).div_ceil(2) } else { 0 };
    }
    let mut output = Vec::with_capacity(destination_length);
    let mut index = 0;
    let fast_limit = destination_length.min(decoded_size) / 3 * 3;
    let mut index_limit = source_length / 4 * 4;
    while index < index_limit && output.len() < fast_limit {
        let values = [0, 1, 2, 3].map(|offset| u32::from(BASE64_TABLE[usize::from(units[index + offset] as u8)]));
        if values.iter().any(|value| *value >= 128) {
            if !base64_slow_group(&units, &mut index, &mut output, destination_length) {
                return output;
            }
            index_limit = index + (source_length - index) / 4 * 4;
        } else {
            output.push(((values[0] << 2 | values[1] >> 4) & 0xff) as u8);
            output.push(((values[1] << 4 | values[2] >> 2) & 0xff) as u8);
            output.push(((values[2] << 6 | values[3]) & 0xff) as u8);
            index += 4;
        }
    }
    if index < source_length && output.len() < destination_length {
        base64_slow_group(&units, &mut index, &mut output, destination_length);
    }
    output
}

/// 표준·URL 두 알파벳을 다 받는 해독표. 255는 잘못된 글자다.
static BASE64_TABLE: LazyLock<[u8; 256]> = LazyLock::new(|| {
    let mut table = [255_u8; 256];
    for (value, character) in b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/".iter().enumerate() {
        table[usize::from(*character)] = value as u8;
    }
    table[usize::from(b'-')] = 62;
    table[usize::from(b'_')] = 63;
    table
});

/// nbytes의 느린 경로. 잘못된 글자를 건너뛰며 네 글자를 모으고, 끝이나 '='나 출력 한도에서 false다.
fn base64_slow_group(units: &[u16], index: &mut usize, output: &mut Vec<u8>, destination_length: usize) -> bool {
    let source_length = units.len();
    let mut high = 0_u8;
    for step in 0..4 {
        let low = loop {
            let character = units[*index] as u8;
            let value = BASE64_TABLE[usize::from(character)];
            *index += 1;
            if value < 64 {
                break value;
            }
            if character == 0x3d || *index >= source_length {
                return false;
            }
        };
        match step {
            1 => output.push(((high & 0x3f) << 2) | ((low & 0x30) >> 4)),
            2 => output.push(((high & 0x0f) << 4) | ((low & 0x3c) >> 2)),
            3 => output.push(((high & 0x03) << 6) | (low & 0x3f)),
            _ => {}
        }
        if *index >= source_length || output.len() >= destination_length {
            return false;
        }
        high = low;
    }
    true
}

fn decoded_jwt_payload(token: Option<&Value>) -> Option<Value> {
    let text = js_string_or_empty(token);
    let payload = text.split('.').nth(1).filter(|part| !part.is_empty())?;
    serde_json::from_str(&String::from_utf8_lossy(&base64url_decode(payload))).ok()
}

/// Node parseCodexSubscriptionExpiration. auth.json id_token claim의 구독 만료 시각만 읽는다.
pub fn parse_codex_subscription_expiration(raw: &str) -> Option<String> {
    let credential: Value = serde_json::from_str(raw).ok()?;
    let claims = decoded_jwt_payload(field(field(Some(&credential), "tokens"), "id_token"))?;
    iso_date(field(field(Some(&claims), "https://api.openai.com/auth"), "chatgpt_subscription_active_until"))
}

fn claude_window(window: Option<&Value>) -> Option<LimitWindow> {
    let window = window.filter(|window| window.is_object() || window.is_array())?;
    Some(LimitWindow {
        remaining: remaining_percent(field(Some(window), "utilization")),
        reset_at: iso_date(field(Some(window), "resets_at")),
    })
}

/// Node parseClaudeRateLimits. weekly_scoped 중 모델 표시 이름이 있는 것만 모델별 주간 한도로 남긴다.
pub fn parse_claude_rate_limits(
    payload: &Value,
    tier: Option<&Value>,
    subscription_type: Option<&Value>,
) -> ClaudeLimits {
    let limits = field(Some(payload), "limits").and_then(Value::as_array).map(Vec::as_slice).unwrap_or_default();
    let model_weekly_limits = limits
        .iter()
        .filter_map(|limit| {
            let model = field(field(Some(limit), "scope"), "model");
            let name = field(model, "display_name");
            if field(Some(limit), "kind").and_then(Value::as_str) != Some("weekly_scoped") || !js_truthy(name) {
                return None;
            }
            let name = name?;
            let id = field(model, "id").filter(|id| js_truthy(Some(id))).unwrap_or(name);
            let percent = field(Some(limit), "percent");
            let missing = matches!(percent, None | Some(Value::Null)) || percent.and_then(Value::as_str) == Some("");
            Some(ModelWeeklyLimit {
                id: js_string(id),
                name: js_string(name),
                remaining: if missing { None } else { remaining_percent(percent) },
                reset_at: iso_date(field(Some(limit), "resets_at")),
            })
        })
        .collect();
    ClaudeLimits {
        five_hour: claude_window(field(Some(payload), "five_hour")),
        weekly: claude_window(field(Some(payload), "seven_day")),
        model_weekly_limits,
        plan: normalized_claude_plan(tier, subscription_type),
    }
}

fn lowercase_text(value: Option<&Value>) -> String {
    js_string_or_empty(value).to_lowercase()
}

fn bucket_matches(bucket: &Value, keys: &[&str]) -> bool {
    let id = lowercase_text(field(Some(bucket), "bucketId"));
    let window = lowercase_text(field(Some(bucket), "window"));
    keys.iter().any(|key| id.contains(key) || window == *key)
}

fn quota_group_id(group: &Value, buckets: &[Value]) -> String {
    let ids: Vec<String> = buckets.iter().map(|bucket| js_string_or_empty(field(Some(bucket), "bucketId"))).collect();
    for (prefix, id) in [("gemini-argon-", "argon"), ("3p-", "third-party"), ("gemini-", "gemini")] {
        if ids.iter().any(|candidate| candidate.starts_with(prefix)) {
            return id.into();
        }
    }
    match ids.first().filter(|id| !id.is_empty()) {
        Some(id) => id.clone(),
        None => present(field(Some(group), "displayName")).map_or_else(|| "unknown".into(), js_string),
    }
}

fn quota_bucket(bucket: &Value) -> QuotaBucket {
    let id = js_string_or_empty(field(Some(bucket), "bucketId"));
    let raw_window = lowercase_text(field(Some(bucket), "window"));
    let week_source = if raw_window.is_empty() { &id } else { &raw_window };
    let window = if FIVE_HOUR_KEYS.iter().any(|key| id.contains(key) || raw_window == *key) {
        "fiveHour".to_string()
    } else if week_source.contains("week") {
        "weekly".to_string()
    } else {
        raw_window
    };
    let fraction = present(field(Some(bucket), "remainingFraction")).and_then(|fraction| finite_number(Some(fraction)));
    QuotaBucket {
        name: present(field(Some(bucket), "displayName")).map_or_else(|| id.clone(), js_string),
        id,
        window,
        remaining: fraction.map(remaining_fraction),
        reset_at: iso_date(field(Some(bucket), "resetTime")),
    }
}

fn parsed_bucket(bucket: Option<&Value>) -> Option<LimitWindow> {
    let fraction = finite_number(Some(present(field(bucket, "remainingFraction"))?))?;
    Some(LimitWindow { remaining: Some(remaining_fraction(fraction)), reset_at: iso_date(field(bucket, "resetTime")) })
}

/// Node parseAntigravityRateLimits. 모든 그룹을 보존하고, 직원 모델(Argon·제3자·일반 Gemini)에 맞는 그룹에서
/// 5시간·주간 값을 고른다. 배열이 아닌 groups·buckets는 Node의 TypeError 문구를 낸다.
pub fn parse_antigravity_rate_limits(payload: &Value, model: &str) -> Result<AntigravityLimits, String> {
    let groups = match present(field(field(Some(payload), "response"), "groups")).or(present(field(Some(payload), "groups")))
    {
        None => &[][..],
        Some(Value::Array(groups)) => groups.as_slice(),
        Some(_) => return Err("groups.map is not a function".into()),
    };
    let mut group_buckets = Vec::with_capacity(groups.len());
    let mut quota_groups = Vec::with_capacity(groups.len());
    for group in groups {
        let buckets = match present(field(Some(group), "buckets")) {
            None => &[][..],
            Some(Value::Array(buckets)) => buckets.as_slice(),
            Some(_) => return Err("((intermediate value) ?? []).map is not a function".into()),
        };
        quota_groups.push(QuotaGroup {
            id: quota_group_id(group, buckets),
            name: js_string_or_empty(field(Some(group), "displayName")),
            buckets: buckets.iter().map(quota_bucket).collect(),
        });
        group_buckets.push(buckets);
    }
    let gemini = (0..groups.len()).find(|index| {
        lowercase_text(field(Some(&groups[*index]), "displayName")) == "gemini models"
            || group_buckets[*index].iter().any(|bucket| {
                ["gemini-weekly", "gemini-5h", "gemini-five-hour"]
                    .contains(&lowercase_text(field(Some(bucket), "bucketId")).as_str())
            })
    });
    let selected = if ARGON_MODEL.is_match(model) {
        (0..groups.len()).find(|index| {
            ARGON_GROUP_NAME.is_match(&js_string_or_empty(field(Some(&groups[*index]), "displayName")))
                || group_buckets[*index]
                    .iter()
                    .any(|bucket| lowercase_text(field(Some(bucket), "bucketId")).starts_with("gemini-argon-"))
        })
    } else if THIRD_PARTY_MODEL.is_match(model) {
        quota_groups.iter().position(|group| group.id == "third-party")
    } else {
        gemini
    };
    let buckets = selected.map(|index| group_buckets[index]).unwrap_or_default();
    let bucket_for = |keys: &[&str]| buckets.iter().find(|bucket| bucket_matches(bucket, keys));
    let five_hour = parsed_bucket(bucket_for(&FIVE_HOUR_KEYS));
    let weekly = parsed_bucket(bucket_for(&["weekly", "week"]));
    if !quota_groups.iter().any(|group| group.buckets.iter().any(|bucket| bucket.remaining.is_some())) {
        return Err("Antigravity 계정 한도 응답에 Gemini 잔여량이 없습니다.".into());
    }
    // 한도 응답은 상품명을 주지 않는다. Gemini 그룹에 5시간 창이 있으면 Google AI Pro, 없으면 무료다.
    let account_has_five_hour = gemini.is_some_and(|index| {
        group_buckets[index]
            .iter()
            .any(|bucket| bucket_matches(bucket, &FIVE_HOUR_KEYS) && parsed_bucket(Some(bucket)).is_some())
    });
    Ok(AntigravityLimits {
        five_hour,
        weekly,
        plan: Some(if account_has_five_hour { "Google AI Pro" } else { "Free" }.into()),
        groups: quota_groups,
        image_reset_at: None,
    })
}

// ── 프로세스 조회 ──

/// Node가 app-server에 쓰는 세 줄(initialize·initialized·account/rateLimits/read). JSON.stringify 결과 그대로다.
const CODEX_REQUEST: &str = concat!(
    r#"{"id":1,"method":"initialize","params":{"clientInfo":{"name":"officestra","version":"1"},"#,
    r#""capabilities":{"experimentalApi":true}}}"#,
    "\n",
    r#"{"method":"initialized","params":null}"#,
    "\n",
    r#"{"id":2,"method":"account/rateLimits/read","params":null}"#,
    "\n",
);

/// Node spawn의 실패 처리. 다섯 오류는 'error' 이벤트라 호출부 문구가 되고, 나머지는 바로 던진 "spawn 코드"다.
fn spawn_failure(error: &io::Error, message: &str) -> String {
    match uv_error(error).0 {
        "EACCES" | "EAGAIN" | "EMFILE" | "ENFILE" | "ENOENT" => message.into(),
        code => format!("spawn {code}"),
    }
}

/// Node child.kill(). 아직 거두지 않은 자식에게 SIGTERM만 보내고 끝나기를 기다리지 않는다.
/// 버린 자식은 tokio가 뒤에서 거둔다.
fn send_terminate(child: &Child) {
    if let Some(pid) = child.id().and_then(|pid| i32::try_from(pid).ok()) {
        // SAFETY: 아직 거두지 않은 자식 프로세스 번호에 신호만 보낸다.
        unsafe { libc::kill(pid, libc::SIGTERM) };
    }
}

/// Node의 `if (stderr.length < 2_048) stderr += chunk`. 조각은 setEncoding("utf8")의 StringDecoder처럼
/// 글자 경계에 걸친 바이트를 다음 조각까지 미루고, 잘못된 바이트는 U+FFFD로 바꾼다.
#[derive(Default)]
struct StderrText {
    text: String,
    pending: Vec<u8>,
}

impl StderrText {
    fn push(&mut self, bytes: &[u8]) {
        self.pending.extend_from_slice(bytes);
        let mut chunk = String::new();
        let mut rest = self.pending.as_slice();
        loop {
            match std::str::from_utf8(rest) {
                Ok(valid) => {
                    chunk.push_str(valid);
                    rest = &[];
                    break;
                }
                Err(error) => {
                    let (valid, invalid) = rest.split_at(error.valid_up_to());
                    chunk.push_str(std::str::from_utf8(valid).unwrap_or_default());
                    match error.error_len() {
                        Some(length) => {
                            chunk.push('\u{FFFD}');
                            rest = &invalid[length..];
                        }
                        None => {
                            rest = invalid;
                            break;
                        }
                    }
                }
            }
        }
        let remainder = rest.to_vec();
        self.pending = remainder;
        self.append(&chunk);
    }

    /// 스트림 끝. 덜 끝난 글자는 StringDecoder.end()처럼 U+FFFD 하나다.
    fn finish(&mut self) {
        if !self.pending.is_empty() {
            self.pending.clear();
            self.append("\u{FFFD}");
        }
    }

    fn append(&mut self, chunk: &str) {
        if !chunk.is_empty() && js_length(&self.text) < 2_048 {
            self.text.push_str(chunk);
        }
    }
}

fn codex_reply(line: &[u8]) -> Option<Result<Option<Value>, String>> {
    // Node는 "null" 줄에서 message.id를 읽다 프로세스가 죽는다. 여기서는 다른 잡음 줄처럼 건너뛴다.
    let message: Value = serde_json::from_str(&String::from_utf8_lossy(line)).ok()?;
    if field(Some(&message), "id")?.as_f64() != Some(2.0) {
        return None;
    }
    Some(if js_truthy(field(Some(&message), "error")) {
        Err("Codex 계정 한도 응답이 실패했습니다.".into())
    } else {
        Ok(field(Some(&message), "result").cloned())
    })
}

/// Node readCodexRateLimits의 app-server 대화. 홈에서 `app-server --stdio`를 띄워 세 줄을 쓰고 stdin은 열어 둔 채
/// id 2 응답을 기다린다. 돌려주는 값은 응답의 result(없으면 None)다. 끝나면 결과와 무관하게 SIGTERM을 보낸다.
/// 자식이 끝나고 stdout·stderr가 모두 닫히면(Node의 close) stderr나 종료 코드로 실패한다.
pub async fn read_codex_app_server(executable: &str, home: &str, timeout: Duration) -> Result<Option<Value>, String> {
    let mut child = tokio::process::Command::new(executable)
        .args(["app-server", "--stdio"])
        .current_dir(home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| spawn_failure(&error, "Codex CLI를 실행할 수 없습니다."))?;
    let mut stdin = child.stdin.take();
    let (mut stdout, mut stderr) = (child.stdout.take(), child.stderr.take());
    let outcome = async {
        if let Some(pipe) = stdin.as_mut()
            && pipe.write_all(CODEX_REQUEST.as_bytes()).await.is_err()
        {
            return Err("Codex CLI 요청을 전달할 수 없습니다.".to_string());
        }
        let deadline = tokio::time::sleep(timeout);
        tokio::pin!(deadline);
        let (mut output_buffer, mut error_buffer) = (vec![0_u8; 65_536], vec![0_u8; 65_536]);
        let mut lines = Vec::new();
        let mut errors = StderrText::default();
        let mut exit: Option<Option<i32>> = None;
        loop {
            if let Some(code) = exit
                && stdout.is_none()
                && stderr.is_none()
            {
                let detail = js_trim(&errors.text);
                return Err(if detail.is_empty() {
                    let code = code.map_or_else(|| "종료".to_string(), |code| code.to_string());
                    format!("Codex 계정 한도 조회가 실패했습니다 ({code}).")
                } else {
                    format!("Codex 계정 한도 조회가 실패했습니다: {detail}")
                });
            }
            tokio::select! {
                biased;
                read = read_open_pipe(&mut stdout, &mut output_buffer), if stdout.is_some() => match read {
                    Ok(0) | Err(_) => stdout = None,
                    Ok(count) => {
                        lines.extend_from_slice(&output_buffer[..count]);
                        while let Some(newline) = lines.iter().position(|byte| *byte == b'\n') {
                            let line: Vec<u8> = lines.drain(..=newline).collect();
                            if let Some(reply) = codex_reply(&line[..newline]) {
                                return reply;
                            }
                        }
                    }
                },
                read = read_open_pipe(&mut stderr, &mut error_buffer), if stderr.is_some() => match read {
                    Ok(0) | Err(_) => {
                        errors.finish();
                        stderr = None;
                    }
                    Ok(count) => errors.push(&error_buffer[..count]),
                },
                status = child.wait(), if exit.is_none() => exit = Some(status.ok().and_then(|status| status.code())),
                () = &mut deadline => return Err("Codex 계정 한도 조회 시간이 초과됐습니다.".to_string()),
            }
        }
    }
    .await;
    send_terminate(&child);
    drop(stdin);
    outcome
}

/// 탐침 중인 agy. 작업을 기다리는 동안에도 stderr를 모으고 종료를 지켜본다(Node의 이벤트 처리기).
struct WatchedProcess {
    child: Child,
    stderr: Option<ChildStderr>,
    errors: StderrText,
    exited: bool,
    buffer: Vec<u8>,
}

impl WatchedProcess {
    /// Node의 close. 자식이 끝나고 stderr가 닫혔다.
    fn closed(&self) -> bool {
        self.exited && self.stderr.is_none()
    }

    async fn during<T>(&mut self, work: impl Future<Output = T>) -> T {
        tokio::pin!(work);
        loop {
            tokio::select! {
                biased;
                output = &mut work => return output,
                read = read_open_pipe(&mut self.stderr, &mut self.buffer), if self.stderr.is_some() => match read {
                    Ok(0) | Err(_) => {
                        self.errors.finish();
                        self.stderr = None;
                    }
                    Ok(count) => self.errors.push(&self.buffer[..count]),
                },
                _ = self.child.wait(), if !self.exited => self.exited = true,
            }
        }
    }
}

/// fs.mkdtemp(prefix). 실패 문구는 Node 오류 message 꼴이다.
pub(crate) fn make_temporary_directory(prefix: &str) -> Result<PathBuf, String> {
    let template = format!("{}/{prefix}XXXXXX", node_tmpdir());
    let mut bytes = CString::new(template.clone()).map_err(|error| error.to_string())?.into_bytes_with_nul();
    // SAFETY: NUL로 끝나는 쓰기 가능한 버퍼를 넘기고, mkdtemp는 끝 여섯 글자만 바꾼다.
    let created = unsafe { libc::mkdtemp(bytes.as_mut_ptr().cast()) };
    if created.is_null() {
        let (code, description) = uv_error(&io::Error::last_os_error());
        return Err(format!("{code}: {description}, mkdtemp '{template}'"));
    }
    bytes.pop();
    Ok(PathBuf::from(OsString::from_vec(bytes)))
}

/// `(payload?.response?.groups ?? []).length > 0`. 배열·문자열 길이, 객체면 length 속성을 숫자로 비교한다.
fn has_quota_groups(payload: &Value) -> bool {
    match present(field(field(Some(payload), "response"), "groups")) {
        Some(Value::Array(groups)) => !groups.is_empty(),
        Some(Value::String(text)) => !text.is_empty(),
        Some(Value::Object(fields)) => fields.get("length").is_some_and(|length| js_to_number(length) > 0.0),
        _ => false,
    }
}

/// fetch의 response.json(). 앞의 BOM을 떼고 잘못된 UTF-8은 U+FFFD로 읽는다.
fn response_json(bytes: &[u8]) -> Result<Value, serde_json::Error> {
    let bytes = bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(bytes);
    serde_json::from_str(&String::from_utf8_lossy(bytes))
}

async fn quota_summary(client: &reqwest::Client, port: u16, token: &str) -> Option<Value> {
    let url = format!(
        "http://127.0.0.1:{port}/exa.language_server_pb.LanguageServerService/RetrieveUserQuotaSummary"
    );
    let response = client
        .post(url)
        .header("content-type", "application/json")
        .header("x-codeium-csrf-token", token)
        .body("{}")
        .send()
        .await
        .ok()?;
    if !response.status().is_success() {
        return None;
    }
    let payload = response_json(&response.bytes().await.ok()?).ok()?;
    has_quota_groups(&payload).then_some(payload)
}

/// Node probeAntigravityQuota. 임시 폴더에 로그를 받게 `agy --csrf_token T --log-file P models`를 띄우고,
/// 로그의 언어 서버 포트로 100ms마다 한도 요약을 묻는다. 환경은 백엔드 것을 물려주고 `extra_environment`를 더한다
/// (Node `antigravityPlaywrightEnvironment(process.env)`에 해당하는 값은 [`antigravity_probe_environment`]가 만든다).
pub async fn probe_antigravity_quota(
    executable: &str,
    home: &str,
    timeout: Duration,
    extra_environment: &BTreeMap<String, String>,
) -> Result<Value, String> {
    let root = make_temporary_directory("officestra-antigravity-quota-")?;
    let result = probe_with_log(executable, home, timeout, &root.join("agy.log"), extra_environment).await;
    let _ = tokio::fs::remove_dir_all(&root).await;
    result
}

/// Node `env: antigravityPlaywrightEnvironment(process.env)`. 드라이버의 package/cli.js가 있으면
/// 백엔드 환경에 아직 없는 PLAYWRIGHT_DRIVER_PATH·PLAYWRIGHT_NODEJS_PATH만 돌려준다(`??=`, 빈 값도 있는 값이다).
pub fn antigravity_probe_environment(sources: &UsageSources) -> BTreeMap<String, String> {
    let Some(driver) = sources.playwright_driver_directory.as_deref() else {
        return BTreeMap::new();
    };
    antigravity_playwright_environment(&BTreeMap::new(), driver, sources.node_executable.as_deref())
        .into_iter()
        .filter(|(key, _)| std::env::var_os(key).is_none())
        .collect()
}

async fn probe_with_log(
    executable: &str,
    home: &str,
    timeout: Duration,
    log_path: &Path,
    extra_environment: &BTreeMap<String, String>,
) -> Result<Value, String> {
    let token = random_uuid();
    let mut child = tokio::process::Command::new(executable)
        .arg("--csrf_token")
        .arg(&token)
        .arg("--log-file")
        .arg(log_path)
        .arg("models")
        .envs(extra_environment)
        .current_dir(home)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| spawn_failure(&error, "Antigravity CLI를 실행할 수 없습니다."))?;
    let stderr = child.stderr.take();
    let mut process = WatchedProcess { child, stderr, errors: StderrText::default(), exited: false, buffer: vec![0; 65_536] };
    let outcome = poll_quota(&mut process, timeout, log_path, &token).await;
    send_terminate(&process.child);
    outcome
}

async fn poll_quota(process: &mut WatchedProcess, timeout: Duration, log_path: &Path, token: &str) -> Result<Value, String> {
    // undici fetch에는 자체 시간 제한이 없고 헤더·본문 대기 기본값이 300초다. 프록시 환경 변수도 쓰지 않는다.
    let client = crate::http_client_builder()
        .no_proxy()
        .timeout(Duration::from_secs(300))
        .build()
        .map_err(|error| error.to_string())?;
    let deadline = Instant::now() + timeout;
    // 바깥 None은 아직 못 찾음, 안쪽 None은 65535를 넘어 URL이 될 수 없는 포트다(Node도 fetch가 계속 실패한다).
    let mut port: Option<Option<u16>> = None;
    while Instant::now() < deadline {
        if port.is_none() {
            let log = process.during(tokio::fs::read(log_path)).await.unwrap_or_default();
            if let Some(captures) = LANGUAGE_SERVER_PORT.captures(&String::from_utf8_lossy(&log)) {
                port = Some(captures[1].parse::<u16>().ok());
            }
        }
        if let Some(Some(port)) = port
            && let Some(payload) = process.during(quota_summary(&client, port, token)).await
        {
            return Ok(payload);
        }
        if process.closed() {
            break;
        }
        process.during(tokio::time::sleep(Duration::from_millis(100))).await;
    }
    let detail = js_trim(&process.errors.text);
    Err(if detail.is_empty() {
        "Antigravity 계정 한도 조회 시간이 초과됐습니다.".into()
    } else {
        format!("Antigravity 계정 한도 조회가 실패했습니다: {detail}")
    })
}

// ── Claude·설정·DB 통계 ──

/// 사용량 조회가 닿는 바깥 자원. 시험은 홈·설정 폴더·Keychain 명령·Claude 주소를 바꿔 실제 계정을 부르지 않는다.
#[derive(Clone)]
pub struct UsageSources {
    pub pool: PgPool,
    /// Node homedir(). Codex·agy 작업 폴더, ~/.claude, ~/.gemini의 기준이다.
    pub home: String,
    pub codex_home: Option<PathBuf>,
    pub claude_config_dir: Option<PathBuf>,
    pub claude_code_oauth_token: Option<Secret>,
    pub keychain_command: String,
    pub claude_usage_url: String,
    pub timeout: Duration,
    /// agy 한도 조회에 넘길 Playwright 드라이버 폴더와 Node 실행 파일(Node는 모듈 옆 드라이버와 process.execPath).
    /// 드라이버 폴더가 None이면 PLAYWRIGHT_* 값을 더하지 않는다.
    pub playwright_driver_directory: Option<String>,
    pub node_executable: Option<String>,
}

impl UsageSources {
    /// 드라이버 폴더와 Node 실행 파일은 실행기 경로(LaunchPaths)와 같은 값을 받는다.
    pub fn from_config(
        pool: PgPool,
        config: &Config,
        playwright_driver_directory: Option<String>,
        node_executable: Option<String>,
    ) -> Self {
        Self {
            pool,
            home: home_directory(),
            codex_home: config.codex_home.clone(),
            claude_config_dir: config.claude_config_dir.clone(),
            claude_code_oauth_token: config.claude_code_oauth_token.clone(),
            keychain_command: "/usr/bin/security".into(),
            claude_usage_url: CLAUDE_USAGE_URL.into(),
            timeout: Duration::from_secs(8),
            playwright_driver_directory,
            node_executable,
        }
    }
}

/// 요금제 판정 재료. 둘 다 원래 JSON 값 그대로다(null·undefined는 None).
#[derive(Clone, Debug, Default, PartialEq)]
struct ClaudePlan {
    tier: Option<Value>,
    subscription_type: Option<Value>,
}

/// Claude Code OAuth 로그인 정보. 토큰이 찍히지 않게 Debug를 두지 않는다.
struct ClaudeCredential {
    access_token: String,
    plan: ClaudePlan,
}

enum CredentialFailure {
    /// JSON.parse의 SyntaxError. 호출부가 위치별 문구로 바꾼다.
    Syntax,
    /// 권한(user:profile)이 없는 로그인 정보. Node는 이 오류를 삼키고 다음 위치를 본다.
    Scope,
}

fn parsed_claude_credential(raw: &str) -> Result<Option<ClaudeCredential>, CredentialFailure> {
    let value: Value = serde_json::from_str(raw).map_err(|_| CredentialFailure::Syntax)?;
    let oauth = present(field(Some(&value), "claudeAiOauth")).unwrap_or(&value);
    let access_token = js_trim(&js_string_or_empty(field(Some(oauth), "accessToken"))).to_string();
    if access_token.is_empty() {
        return Ok(None);
    }
    if let Some(scopes) = field(Some(oauth), "scopes").and_then(Value::as_array)
        && !scopes.is_empty()
        && !scopes.iter().any(|scope| scope.as_str() == Some("user:profile"))
    {
        return Err(CredentialFailure::Scope);
    }
    Ok(Some(ClaudeCredential {
        access_token,
        plan: ClaudePlan {
            tier: present(field(Some(oauth), "rateLimitTier")).cloned(),
            subscription_type: present(field(Some(oauth), "subscriptionType")).cloned(),
        },
    }))
}

/// Node readClaudeCredential. 환경 토큰 → `<CLAUDE_CONFIG_DIR ?? ~/.claude>/.credentials.json` → Keychain 순서다.
async fn read_claude_credential(sources: &UsageSources) -> Result<ClaudeCredential, String> {
    if let Some(token) = &sources.claude_code_oauth_token {
        let token = js_trim(token.expose());
        if !token.is_empty() {
            return Ok(ClaudeCredential { access_token: token.into(), plan: ClaudePlan::default() });
        }
    }
    let directory = sources.claude_config_dir.clone().unwrap_or_else(|| Path::new(&sources.home).join(".claude"));
    if let Ok(bytes) = tokio::fs::read(directory.join(".credentials.json")).await {
        match parsed_claude_credential(&String::from_utf8_lossy(&bytes)) {
            Ok(Some(credential)) => return Ok(credential),
            Err(CredentialFailure::Syntax) => return Err("Claude Code 로그인 정보를 읽을 수 없습니다.".into()),
            Ok(None) | Err(CredentialFailure::Scope) => {}
        }
    }
    let arguments = ["find-generic-password", "-s", "Claude Code-credentials", "-w"];
    if let Ok(output) =
        exec_file(&sources.keychain_command, &arguments, None, Duration::from_secs(3), DEFAULT_MAX_BUFFER).await
    {
        match parsed_claude_credential(&output.stdout) {
            Ok(Some(credential)) => return Ok(credential),
            Err(CredentialFailure::Syntax) => {
                return Err("Claude Code Keychain 로그인 정보를 읽을 수 없습니다.".into());
            }
            Ok(None) | Err(CredentialFailure::Scope) => {}
        }
    }
    Err("Claude Code 로그인 정보를 찾을 수 없습니다.".into())
}

/// Node readClaudeAccountPlan. 로그인 정보의 rateLimitTier는 요금제를 바꿔도 남으므로,
/// Claude Code가 고쳐 쓰는 .claude.json의 oauthAccount가 있으면 그쪽을 따른다.
async fn read_claude_account_plan(sources: &UsageSources) -> Option<ClaudePlan> {
    let path = match sources.claude_config_dir.as_ref().filter(|directory| !directory.as_os_str().is_empty()) {
        Some(directory) => directory.join(".claude.json"),
        None => Path::new(&sources.home).join(".claude.json"),
    };
    let bytes = tokio::fs::read(path).await.ok()?;
    let config: Value = serde_json::from_str(&String::from_utf8_lossy(&bytes)).ok()?;
    let account = field(Some(&config), "oauthAccount");
    let organization = js_string_or_empty(field(account, "organizationType"));
    let organization = js_trim(&organization);
    let kind = organization.strip_prefix("claude_").unwrap_or(organization);
    if kind.is_empty() {
        return None;
    }
    Some(ClaudePlan {
        tier: present(field(account, "organizationRateLimitTier")).cloned(),
        subscription_type: Some(Value::String(kind.into())),
    })
}

/// Node readClaudeRateLimits. OAuth 토큰으로 사용량 주소를 읽기 전용으로 부른다. 시간 제한은 본문까지 포함한다.
async fn read_claude_rate_limits(sources: &UsageSources) -> Result<ClaudeLimits, String> {
    let credential = read_claude_credential(sources).await?;
    let plan = match read_claude_account_plan(sources).await {
        Some(plan) => plan,
        None => credential.plan.clone(),
    };
    let client = crate::http_client_builder().no_proxy().build().map_err(|_| "fetch failed".to_string())?;
    let request = async {
        let response = client
            .get(&sources.claude_usage_url)
            .header("authorization", format!("Bearer {}", credential.access_token))
            .header("anthropic-beta", "oauth-2025-04-20")
            .header("user-agent", "OFFICESTRA/1")
            .send()
            .await
            .map_err(|_| "fetch failed".to_string())?;
        let status = response.status();
        if !status.is_success() {
            return Err(format!("Claude Code 계정 한도 조회가 실패했습니다 (HTTP {}).", status.as_u16()));
        }
        let bytes = response.bytes().await.map_err(|_| "fetch failed".to_string())?;
        let payload = response_json(&bytes).map_err(|error| error.to_string())?;
        Ok(parse_claude_rate_limits(&payload, plan.tier.as_ref(), plan.subscription_type.as_ref()))
    };
    tokio::time::timeout(sources.timeout, request)
        .await
        .unwrap_or_else(|_| Err("Claude Code 계정 한도 조회 시간이 초과됐습니다.".into()))
}

/// Node readAntigravityImageCooldown. 파일의 resetAt이 now보다 뒤일 때만 ISO 시각이다.
pub async fn read_antigravity_image_cooldown(home: &str, now: f64) -> Option<String> {
    let path = Path::new(home).join(".gemini").join("antigravity-cli").join("image-cooldown.json");
    let bytes = tokio::fs::read(path).await.ok()?;
    let parsed: Value = serde_json::from_str(&String::from_utf8_lossy(&bytes)).ok()?;
    let reset_at = field(Some(&parsed), "resetAt").filter(|value| js_truthy(Some(value)))?;
    let time = date_time(reset_at, 1.0)?;
    (time > now).then(|| iso_string(time))
}

const CODEX_EXECUTABLE_SQL: &str = "
      SELECT config ->> 'executablePath' AS path
      FROM characters
      WHERE backend = 'codex'
        AND NULLIF(config ->> 'executablePath', '') IS NOT NULL
      ORDER BY updated_at DESC, id
      LIMIT 1
    ";

const ANTIGRAVITY_SETTINGS_SQL: &str = "
      SELECT config ->> 'executablePath' AS path, model
      FROM characters
      WHERE backend = 'antigravity'
      ORDER BY updated_at DESC, id
      LIMIT 1
    ";

fn trimmed_or(value: Option<String>, fallback: &str) -> String {
    let value = value.unwrap_or_default();
    let trimmed = js_trim(&value);
    if trimmed.is_empty() { fallback.into() } else { trimmed.into() }
}

/// Node configuredCodexExecutable. 실행 경로를 지정한 가장 최근 Codex 직원의 값, 없으면 codex다.
pub async fn configured_codex_executable(pool: &PgPool) -> Result<String, String> {
    let path: Option<Option<String>> =
        sqlx::query_scalar(CODEX_EXECUTABLE_SQL).fetch_optional(pool).await.map_err(|error| database_message(&error))?;
    Ok(trimmed_or(path.flatten(), "codex"))
}

/// Node configuredAntigravitySettings. 가장 최근 Antigravity 직원의 실행 경로(없으면 agy)와 모델이다.
pub async fn configured_antigravity_settings(pool: &PgPool) -> Result<(String, String), String> {
    let row: Option<(Option<String>, Option<String>)> = sqlx::query_as(ANTIGRAVITY_SETTINGS_SQL)
        .fetch_optional(pool)
        .await
        .map_err(|error| database_message(&error))?;
    let (path, model) = row.unwrap_or_default();
    Ok((trimmed_or(path, "agy"), trimmed_or(model, "")))
}

async fn read_codex_rate_limits(sources: &UsageSources) -> Result<CodexLimits, String> {
    let executable = configured_codex_executable(&sources.pool).await?;
    let result = read_codex_app_server(&executable, &sources.home, sources.timeout).await?;
    let mut limits = parse_codex_rate_limits(result.as_ref());
    limits.subscription_expires_at = read_codex_subscription_expiration(sources).await;
    Ok(limits)
}

/// Node readCodexSubscriptionExpiration. `<CODEX_HOME(공백 제거) || ~/.codex>/auth.json`을 읽고, 실패는 null이다.
async fn read_codex_subscription_expiration(sources: &UsageSources) -> Option<String> {
    let configured = sources.codex_home.as_ref().map(|path| js_trim(&path.to_string_lossy()).to_string());
    let directory = match configured.filter(|path| !path.is_empty()) {
        Some(path) => PathBuf::from(path),
        None => Path::new(&sources.home).join(".codex"),
    };
    let bytes = tokio::fs::read(directory.join("auth.json")).await.ok()?;
    parse_codex_subscription_expiration(&String::from_utf8_lossy(&bytes))
}

async fn read_antigravity_rate_limits(sources: &UsageSources) -> Result<AntigravityLimits, String> {
    let (executable, model) = configured_antigravity_settings(&sources.pool).await?;
    let payload =
        probe_antigravity_quota(&executable, &sources.home, sources.timeout, &antigravity_probe_environment(sources))
            .await?;
    let mut limits = parse_antigravity_rate_limits(&payload, &model)?;
    limits.image_reset_at = read_antigravity_image_cooldown(&sources.home, now_milliseconds()).await;
    Ok(limits)
}

/// 공급자 하나의 오늘·30일 비용과 토큰. 비용은 CLI가 보고한 값의 합이다.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ProviderActivity {
    #[serde(rename = "todayCostUSD")]
    pub today_cost_usd: f64,
    #[serde(rename = "recentTokens")]
    pub recent_tokens: f64,
    #[serde(rename = "last30DaysCostUSD")]
    pub last_30_days_cost_usd: f64,
    #[serde(rename = "last30DaysTokens")]
    pub last_30_days_tokens: f64,
    #[serde(rename = "costEstimateSupported")]
    pub cost_estimate_supported: bool,
}

impl Default for ProviderActivity {
    fn default() -> Self {
        Self {
            today_cost_usd: 0.0,
            recent_tokens: 0.0,
            last_30_days_cost_usd: 0.0,
            last_30_days_tokens: 0.0,
            cost_estimate_supported: true,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct UsageActivity {
    pub codex: ProviderActivity,
    pub claude: ProviderActivity,
    pub antigravity: ProviderActivity,
}

/// Node readUsageActivity의 SQL. 시각 인자는 ISO 문자열을 timestamptz로 바꿔 넣는다.
const ACTIVITY_SQL: &str = r#"
      WITH measured AS (
        SELECT
          COALESCE(NULLIF(turn_record.backend, ''), character.backend)
            AS provider,
          turn_record.started_at,
          COALESCE(usage.cost_usd, 0) AS cost_usd,
          CASE
            WHEN COALESCE(NULLIF(turn_record.backend, ''), character.backend)
              IN ('claude', 'antigravity')
            THEN
              COALESCE(usage.input_tokens, 0)
              + COALESCE(usage.output_tokens, 0)
              + COALESCE(usage.cached_input_tokens, 0)
              + COALESCE(usage.cache_write_input_tokens, 0)
            ELSE
              COALESCE(usage.input_tokens, 0)
              + COALESCE(usage.output_tokens, 0)
          END AS token_count
        FROM usage_records AS usage
        JOIN turns AS turn_record ON turn_record.id = usage.turn_id
        JOIN cli_sessions AS session
          ON session.id = turn_record.cli_session_id
        JOIN characters AS character
          ON character.id = session.character_id
        WHERE turn_record.started_at >= $2::timestamptz
          AND turn_record.provider_kind = 'cloud'
      )
      SELECT
        provider,
        COALESCE(
          SUM(cost_usd) FILTER (WHERE started_at >= $1::timestamptz),
          0
        )::double precision AS "todayCostUSD",
        COALESCE(
          SUM(token_count) FILTER (WHERE started_at >= $1::timestamptz),
          0
        )::double precision AS "recentTokens",
        COALESCE(SUM(cost_usd), 0)::double precision
          AS "last30DaysCostUSD",
        COALESCE(SUM(token_count), 0)::double precision
          AS "last30DaysTokens"
      FROM measured
      GROUP BY provider
    "#;

type ActivityRow = (Option<String>, Option<f64>, Option<f64>, Option<f64>, Option<f64>);

/// 공급자별 행을 숫자로 정규화한다. null·NaN·무한대는 0이고 토큰은 Math.round다. 모르는 공급자는 버린다.
fn activity_from_rows(rows: Vec<ActivityRow>) -> UsageActivity {
    let finite = |value: Option<f64>| value.filter(|value| value.is_finite()).unwrap_or(0.0);
    let mut activity = UsageActivity::default();
    for (provider, today_cost, recent_tokens, cost, tokens) in rows {
        let target = match provider.as_deref() {
            Some("codex") => &mut activity.codex,
            Some("claude") => &mut activity.claude,
            Some("antigravity") => &mut activity.antigravity,
            _ => continue,
        };
        *target = ProviderActivity {
            today_cost_usd: finite(today_cost),
            recent_tokens: js_round(finite(recent_tokens)),
            last_30_days_cost_usd: finite(cost),
            last_30_days_tokens: js_round(finite(tokens)),
            cost_estimate_supported: true,
        };
    }
    activity
}

/// `new Date(y, m, d)`. now가 속한 실행 지역 날짜의 자정이다.
fn local_day_start(now: f64) -> f64 {
    let seconds = (now / 1_000.0).floor() as libc::time_t;
    // SAFETY: localtime_r·mktime은 넘긴 구조체만 읽고 쓴다.
    unsafe {
        let mut parts: libc::tm = std::mem::zeroed();
        libc::localtime_r(&seconds, &mut parts);
        parts.tm_hour = 0;
        parts.tm_min = 0;
        parts.tm_sec = 0;
        parts.tm_isdst = -1;
        libc::mktime(&mut parts) as f64 * 1_000.0
    }
}

/// Node readUsageActivity. 오늘(실행 지역 자정부터)과 최근 30일의 클라우드 사용량을 공급자별로 합친다.
pub async fn read_usage_activity(pool: &PgPool, now: f64) -> Result<UsageActivity, String> {
    let rows: Vec<ActivityRow> = sqlx::query_as(ACTIVITY_SQL)
        .bind(iso_string(local_day_start(now)))
        .bind(iso_string(now - 30.0 * 86_400_000.0))
        .fetch_all(pool)
        .await
        .map_err(|error| database_message(&error))?;
    Ok(activity_from_rows(rows))
}

// ── 요약기 ──

/// 공급자 한도 읽기. Ok(None)은 Node 읽기 함수가 null로 끝난 경우(건너뜀과 같게 다룬다)다.
pub type ProviderRead<T> = Arc<dyn Fn() -> BoxFuture<'static, Result<Option<T>, String>> + Send + Sync>;
pub type ActivityRead = Arc<dyn Fn(f64) -> BoxFuture<'static, Result<UsageActivity, String>> + Send + Sync>;

/// createUsageSummaryReader가 받는 읽기 함수들.
#[derive(Clone)]
pub struct UsageProviders {
    pub codex: ProviderRead<CodexLimits>,
    pub claude: ProviderRead<ClaudeLimits>,
    pub antigravity: ProviderRead<AntigravityLimits>,
    pub activity: ActivityRead,
}

impl UsageProviders {
    /// 실제 Codex app-server·Claude 사용량 주소·agy·DB를 읽는 기본 구성이다.
    pub fn system(sources: UsageSources) -> Self {
        let sources = Arc::new(sources);
        let (codex, claude, antigravity, activity) =
            (Arc::clone(&sources), Arc::clone(&sources), Arc::clone(&sources), sources);
        Self {
            codex: Arc::new(move || {
                let sources = Arc::clone(&codex);
                async move { read_codex_rate_limits(&sources).await.map(Some) }.boxed()
            }),
            claude: Arc::new(move || {
                let sources = Arc::clone(&claude);
                async move { read_claude_rate_limits(&sources).await.map(Some) }.boxed()
            }),
            antigravity: Arc::new(move || {
                let sources = Arc::clone(&antigravity);
                async move { read_antigravity_rate_limits(&sources).await.map(Some) }.boxed()
            }),
            activity: Arc::new(move |now| {
                let sources = Arc::clone(&activity);
                async move { read_usage_activity(&sources.pool, now).await }.boxed()
            }),
        }
    }
}

/// 시계와 캐시 수명(밀리초). 기본값은 Node와 같다.
#[derive(Clone)]
pub struct UsageSummaryOptions {
    pub now: Arc<dyn Fn() -> f64 + Send + Sync>,
    pub cache_lifetime: f64,
    /// Claude 한도 주소는 토큰당 호출 간격이 좁아 직원 CLI와 함께 자주 부르면 429가 난다.
    pub claude_cache_lifetime: f64,
    pub claude_retry_backoff: f64,
    pub antigravity_cache_lifetime: f64,
    pub antigravity_retry_backoff: f64,
}

impl Default for UsageSummaryOptions {
    fn default() -> Self {
        Self {
            now: Arc::new(now_milliseconds),
            cache_lifetime: 30_000.0,
            claude_cache_lifetime: 300_000.0,
            claude_retry_backoff: 90_000.0,
            antigravity_cache_lifetime: 300_000.0,
            antigravity_retry_backoff: 90_000.0,
        }
    }
}

type LimitSnapshot = Arc<Map<String, Value>>;

#[derive(Default)]
struct ReaderState {
    cached: Option<(f64, LimitSnapshot)>,
    in_flight: Option<Shared<BoxFuture<'static, LimitSnapshot>>>,
    last_good_codex: Option<CodexLimits>,
    last_good_claude: Option<ClaudeLimits>,
    last_good_antigravity: Option<AntigravityLimits>,
    claude_cooldown_until: f64,
    antigravity_cooldown_until: f64,
}

/// Node createUsageSummaryReader. 한도는 짧게 캐시하고 동시 요청은 갱신 하나를 같이 기다린다.
/// DB 통계는 매 요청 새로 읽는다.
pub struct UsageSummaryReader {
    providers: UsageProviders,
    options: UsageSummaryOptions,
    state: Mutex<ReaderState>,
}

struct Resolved<T> {
    value: Option<T>,
    error: Option<String>,
    stale: bool,
}

/// Node resolvedProviderLimits. 실패하거나 건너뛰면 직전 정상값을 유지하고 오래된 값으로 표시한다.
fn resolved<T: Clone>(result: Option<Result<Option<T>, String>>, last_good: &Option<T>, fallback: &str) -> Resolved<T> {
    match result {
        Some(Ok(Some(value))) => Resolved { value: Some(value), error: None, stale: false },
        Some(Err(error)) => {
            let message = js_trim(&error);
            let error = if message.is_empty() { fallback.to_string() } else { message.to_string() };
            Resolved { value: last_good.clone(), error: Some(error), stale: last_good.is_some() }
        }
        Some(Ok(None)) | None => Resolved { value: last_good.clone(), error: None, stale: last_good.is_some() },
    }
}

fn window_values(window: Option<&LimitWindow>) -> (Option<i64>, Option<String>) {
    (window.and_then(|window| window.remaining), window.and_then(|window| window.reset_at.clone()))
}

/// Node flattenedLimits. 키 순서도 Node와 같다.
fn flattened_limits(
    codex: &Resolved<CodexLimits>,
    claude: &Resolved<ClaudeLimits>,
    antigravity: &Resolved<AntigravityLimits>,
    fetched_at: f64,
) -> Map<String, Value> {
    let (codex_value, claude_value, antigravity_value) =
        (codex.value.as_ref(), claude.value.as_ref(), antigravity.value.as_ref());
    let (codex_five_hour, codex_five_hour_reset) = window_values(codex_value.and_then(|value| value.five_hour.as_ref()));
    let (codex_weekly, codex_weekly_reset) = window_values(codex_value.and_then(|value| value.weekly.as_ref()));
    let (claude_five_hour, claude_five_hour_reset) =
        window_values(claude_value.and_then(|value| value.five_hour.as_ref()));
    let (claude_weekly, claude_weekly_reset) = window_values(claude_value.and_then(|value| value.weekly.as_ref()));
    let (antigravity_five_hour, antigravity_five_hour_reset) =
        window_values(antigravity_value.and_then(|value| value.five_hour.as_ref()));
    let (antigravity_weekly, antigravity_weekly_reset) =
        window_values(antigravity_value.and_then(|value| value.weekly.as_ref()));
    let summary = json!({
        "codexFiveHour": codex_five_hour,
        "codexFiveHourResetAt": codex_five_hour_reset,
        "codexWeekly": codex_weekly,
        "codexWeeklyResetAt": codex_weekly_reset,
        "claudeFiveHour": claude_five_hour,
        "claudeFiveHourResetAt": claude_five_hour_reset,
        "claudeWeekly": claude_weekly,
        "claudeWeeklyResetAt": claude_weekly_reset,
        "claudeModelWeeklyLimits": claude_value.map(|value| value.model_weekly_limits.clone()).unwrap_or_default(),
        "antigravityFiveHour": antigravity_five_hour,
        "antigravityFiveHourResetAt": antigravity_five_hour_reset,
        "antigravityWeekly": antigravity_weekly,
        "antigravityWeeklyResetAt": antigravity_weekly_reset,
        "antigravityQuotaGroups": antigravity_value.map(|value| value.groups.clone()).unwrap_or_default(),
        "antigravityImageResetAt": antigravity_value.and_then(|value| value.image_reset_at.clone()),
        "codexPlan": codex_value.and_then(|value| value.plan.clone()),
        "claudePlan": claude_value.and_then(|value| value.plan.clone()),
        "antigravityPlan": antigravity_value.and_then(|value| value.plan.clone()),
        "codexSubscriptionExpiresAt": codex_value.and_then(|value| value.subscription_expires_at.clone()),
        "codexLimitError": codex.error,
        "claudeLimitError": claude.error,
        "antigravityLimitError": antigravity.error,
        "codexLimitStale": codex.stale,
        "claudeLimitStale": claude.stale,
        "antigravityLimitStale": antigravity.stale,
        "fetchedAt": iso_string(fetched_at),
    });
    match summary {
        Value::Object(fields) => fields,
        _ => Map::new(),
    }
}

async fn optional<T>(read: Option<BoxFuture<'static, T>>) -> Option<T> {
    match read {
        Some(read) => Some(read.await),
        None => None,
    }
}

impl UsageSummaryReader {
    pub fn new(providers: UsageProviders, options: UsageSummaryOptions) -> Arc<Self> {
        Arc::new(Self { providers, options, state: Mutex::default() })
    }

    fn state(&self) -> MutexGuard<'_, ReaderState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Node usageSummary({ force }). 한도와 DB 통계를 함께 읽고, DB 통계가 실패하면 그 오류 문구다.
    pub async fn read(self: &Arc<Self>, force: bool) -> Result<Value, String> {
        let current = (self.options.now)();
        let activity = (self.providers.activity)(current);
        let (limits, activity) = tokio::try_join!(self.limits(force).map(Ok::<_, String>), activity)?;
        let mut summary = (*limits).clone();
        for (key, value) in [
            ("codexActivity", activity.codex),
            ("claudeActivity", activity.claude),
            ("antigravityActivity", activity.antigravity),
        ] {
            summary.insert(key.into(), serde_json::to_value(value).unwrap_or(Value::Null));
        }
        Ok(Value::Object(summary))
    }

    async fn limits(self: &Arc<Self>, force: bool) -> LimitSnapshot {
        let current = (self.options.now)();
        let in_flight = {
            let mut state = self.state();
            if !force
                && let Some((cached_at, value)) = &state.cached
                && current - cached_at < self.options.cache_lifetime
            {
                return Arc::clone(value);
            }
            match &state.in_flight {
                Some(in_flight) => in_flight.clone(),
                None => {
                    let (task_reader, waiting_reader) = (Arc::clone(self), Arc::clone(self));
                    let task = tokio::spawn(async move { task_reader.refresh(current).await });
                    let shared = async move {
                        match task.await {
                            Ok(snapshot) => snapshot,
                            Err(error) => {
                                waiting_reader.state().in_flight = None;
                                std::panic::resume_unwind(error.into_panic())
                            }
                        }
                    }
                    .boxed()
                    .shared();
                    state.in_flight = Some(shared.clone());
                    shared
                }
            }
        };
        in_flight.await
    }

    /// 성공 뒤에는 캐시 수명, 실패 뒤에는 백오프가 지나야 Claude·Antigravity를 다시 부른다. force여도 지킨다.
    async fn refresh(&self, current: f64) -> LimitSnapshot {
        let (claude_due, antigravity_due) = {
            let state = self.state();
            (current >= state.claude_cooldown_until, current >= state.antigravity_cooldown_until)
        };
        let codex = (self.providers.codex)();
        let claude = claude_due.then(|| (self.providers.claude)());
        let antigravity = antigravity_due.then(|| (self.providers.antigravity)());
        let (codex, claude, antigravity) = tokio::join!(codex, optional(claude), optional(antigravity));
        let fetched_at = (self.options.now)();
        let options = &self.options;
        let mut state = self.state();
        if claude_due {
            let lifetime =
                if matches!(claude, Some(Ok(Some(_)))) { options.claude_cache_lifetime } else { options.claude_retry_backoff };
            state.claude_cooldown_until = fetched_at + lifetime;
        }
        if antigravity_due {
            let lifetime = if matches!(antigravity, Some(Ok(Some(_)))) {
                options.antigravity_cache_lifetime
            } else {
                options.antigravity_retry_backoff
            };
            state.antigravity_cooldown_until = fetched_at + lifetime;
        }
        let codex = resolved(Some(codex), &state.last_good_codex, "Codex 한도를 확인할 수 없습니다.");
        let claude = resolved(claude, &state.last_good_claude, "Claude Code 한도를 확인할 수 없습니다.");
        let antigravity = resolved(antigravity, &state.last_good_antigravity, "Antigravity 한도를 확인할 수 없습니다.");
        if let Some(value) = &codex.value {
            state.last_good_codex = Some(value.clone());
        }
        if let Some(value) = &claude.value {
            state.last_good_claude = Some(value.clone());
        }
        if let Some(value) = &antigravity.value {
            state.last_good_antigravity = Some(value.clone());
        }
        let snapshot = Arc::new(flattened_limits(&codex, &claude, &antigravity, fetched_at));
        state.cached = Some((fetched_at, Arc::clone(&snapshot)));
        state.in_flight = None;
        snapshot
    }
}

#[cfg(test)]
mod tests;
