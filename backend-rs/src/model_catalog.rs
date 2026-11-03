// 설치된 Codex·Antigravity·Claude CLI의 모델 목록과 모델별 실행 옵션을 수집한다. Node model-catalog.mjs를 옮겼다.
//
// 모델 한 줄은 CatalogModel이고 JSON 키 순서는 Node model()이 만드는 객체와 같다. 저장된 JSON과 CLI 출력은
// normalized_model()이 Node의 String·Number 변환 규칙대로 정리한다. 정리된 모델은 다시 정리해도 그대로다.

use std::collections::{HashMap, HashSet};
use std::os::unix::fs::MetadataExt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock, Mutex, PoisonError};
use std::time::Duration;

use futures_util::future::BoxFuture;
use futures_util::stream::{FuturesUnordered, StreamExt};
use regex::Regex;
use serde::Serialize;
use serde_json::{Value, json};
use sqlx::PgPool;

use crate::agent_runtime::locate_executable;
use crate::antigravity_model_selection::is_antigravity_fixed_thinking_model;
use crate::child_process::exec_file;
use crate::js::{
    date_parse, iso_string, js_max, js_string, js_string_or_empty, js_to_number, js_trim, now_milliseconds, to_js_json,
    utf16_prefix,
};

pub const MODEL_CATALOG_REFRESH_MILLISECONDS: f64 = 12.0 * 60.0 * 60.0 * 1_000.0;
pub const MODEL_CATALOG_PROVIDERS: [&str; 3] = ["codex", "antigravity", "claude"];
pub const COMMAND_TIMEOUT: Duration = Duration::from_millis(30_000);
pub const MAX_COMMAND_OUTPUT_BYTES: usize = 2 * 1_024 * 1_024;
const EFFORT_ORDER: [&str; 8] = ["none", "minimal", "low", "medium", "high", "xhigh", "max", "ultra"];
const MAX_SAFE_INTEGER: f64 = 9_007_199_254_740_991.0;
const MAX_TIME_MILLISECONDS: f64 = 8.64e15;

/// Claude Code는 모델 나열 명령이 없지만 stream-json 입력으로 SDK initialize 요청을 받으면 고를 수 있는
/// 모델 목록을 control_response로 돌려준다. 프롬프트를 보내지 않으므로 API 호출과 비용이 생기지 않는다.
pub const CLAUDE_INITIALIZE_REQUEST: &str =
    "{\"type\":\"control_request\",\"request_id\":\"officestra-model-catalog\",\"request\":{\"subtype\":\"initialize\"}}\n";

/// 제공자별 목록 명령과 해석기. Node SOURCES와 같다.
pub struct CatalogSource {
    pub provider: &'static str,
    pub arguments: &'static [&'static str],
    pub input: Option<&'static str>,
    pub parse: fn(&str) -> Result<ModelCatalog, String>,
}

static CATALOG_SOURCES: [CatalogSource; 3] = [
    CatalogSource { provider: "codex", arguments: &["debug", "models"], input: None, parse: parse_codex_model_catalog },
    CatalogSource { provider: "antigravity", arguments: &["models"], input: None, parse: parse_antigravity_model_catalog },
    CatalogSource {
        provider: "claude",
        arguments: &["-p", "--input-format", "stream-json", "--output-format", "stream-json", "--verbose"],
        input: Some(CLAUDE_INITIALIZE_REQUEST),
        parse: parse_claude_model_catalog,
    },
];

pub fn catalog_source(provider: &str) -> Option<&'static CatalogSource> {
    CATALOG_SOURCES.iter().find(|source| source.provider == provider)
}

/// Node model()이 만드는 모델 한 줄. 필드 순서가 JSON 키 순서다.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CatalogModel {
    pub id: String,
    pub title: String,
    pub efforts: Vec<String>,
    pub default_effort: String,
    pub supports_fast_mode: bool,
    pub context_window: Option<i64>,
    pub max_context_window: Option<i64>,
    pub available: bool,
    pub resolved_model: Option<String>,
    pub previous_resolved_model: Option<String>,
    pub resolved_model_changed_at: Option<String>,
}

/// `{ version: 1, models }`.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ModelCatalog {
    pub version: u8,
    pub models: Vec<CatalogModel>,
}

impl Default for ModelCatalog {
    fn default() -> Self {
        Self::new(Vec::new())
    }
}

impl ModelCatalog {
    pub fn new(models: Vec<CatalogModel>) -> Self {
        Self { version: 1, models }
    }

    pub fn to_json(&self) -> Value {
        serde_json::to_value(self).unwrap_or(Value::Null)
    }

    /// 정리된 모델 목록의 normalizeCatalog. 다시 정리해도 같으므로 같은 식별자의 첫 항목만 남긴다.
    fn deduplicated(&self) -> Self {
        let mut seen = HashSet::new();
        Self::new(self.models.iter().filter(|entry| seen.insert(entry.id.clone())).cloned().collect())
    }
}

/// normalizeCatalog. models 배열의 모델을 정리하고 같은 식별자는 첫 항목만 남긴다.
pub fn normalize_catalog(value: &Value) -> ModelCatalog {
    let mut seen = HashSet::new();
    let models = value
        .get("models")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(normalized_model)
        .filter(|entry| seen.insert(entry.id.clone()))
        .collect();
    ModelCatalog::new(models)
}

/// builtinModelCatalog. CLI 목록을 아직 받지 못했을 때 쓰는 내장 기본값이다.
pub fn builtin_model_catalog(provider: &str) -> Option<ModelCatalog> {
    let entry = |id: &str, title: &str, efforts: &[&str], default_effort: &str, fast: bool| {
        normalized_model(&json!({
            "id": id, "title": title, "efforts": efforts, "defaultEffort": default_effort, "supportsFastMode": fast,
        }))
    };
    let all = ["low", "medium", "high", "xhigh", "max", "ultra"];
    let claude = ["high", "xhigh", "max"];
    let models = match provider {
        "codex" => vec![
            entry("gpt-5.6-sol", "GPT-5.6-Sol", &all, "high", true),
            entry("gpt-5.6-terra", "GPT-5.6-Terra", &all, "high", true),
            entry("gpt-5.6-luna", "GPT-5.6-Luna", &all[..5], "medium", true),
        ],
        "antigravity" => vec![
            entry("gemini-3.8-flash", "Gemini 3.8 Flash", &["low", "medium", "high"], "high", false),
            entry("gemini-3.1-pro", "Gemini 3.1 Pro", &["low", "high"], "high", false),
        ],
        "claude" => vec![
            entry("claude-opus-5", "Opus 5", &claude, "high", true),
            entry("fable", "Fable", &claude, "high", false),
            entry("claude-sonnet-5", "Sonnet 5", &claude, "high", false),
        ],
        _ => return None,
    };
    Some(ModelCatalog::new(models.into_iter().flatten().collect()))
}

/// Node model(). 식별자가 올바르지 않거나 추론 단계가 하나도 없으면 None이다. JSON 객체가 아니면 모든 칸이 없는 것과 같다.
pub fn normalized_model(entry: &Value) -> Option<CatalogModel> {
    let fields = entry.as_object()?;
    let present = |key: &str| fields.get(key).filter(|value| !value.is_null());
    let id = js_string(present("id")?);
    if !valid_model_id(&id) {
        return None;
    }
    let efforts = unique_efforts(fields.get("efforts"));
    if efforts.is_empty() {
        return None;
    }
    let title = present("title").map_or_else(|| id.clone(), js_string);
    let title = match js_trim(&title) {
        "" => id.clone(),
        trimmed => trimmed.to_string(),
    };
    let default_effort = match fields.get("defaultEffort").and_then(Value::as_str) {
        Some(effort) if efforts.iter().any(|known| known == effort) => effort.to_string(),
        _ => preferred_effort(&efforts),
    };
    Some(CatalogModel {
        id,
        title,
        efforts,
        default_effort,
        supports_fast_mode: fields.get("supportsFastMode") == Some(&Value::Bool(true)),
        context_window: safe_positive_integer(fields.get("contextWindow")),
        max_context_window: safe_positive_integer(fields.get("maxContextWindow")),
        available: fields.get("available") != Some(&Value::Bool(false)),
        resolved_model: optional_text(fields.get("resolvedModel")),
        previous_resolved_model: optional_text(fields.get("previousResolvedModel")),
        resolved_model_changed_at: optional_timestamp(fields.get("resolvedModelChangedAt")),
    })
}

/// `String(value ?? "").trim() || null`.
fn optional_text(value: Option<&Value>) -> Option<String> {
    let text = js_string_or_empty(value);
    let trimmed = js_trim(&text);
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

/// optionalTimestamp. 글자는 Date.parse, 그 밖은 Number로 읽어 ISO 글자로 쓴다.
/// Node는 Date 범위를 넘는 수에서 RangeError를 내지만 이 칸에는 ISO 글자만 저장되므로 비워 둔다.
fn optional_timestamp(value: Option<&Value>) -> Option<String> {
    let time = match value? {
        Value::Null => return None,
        Value::String(text) if text.is_empty() => return None,
        Value::String(text) => date_parse(text)?,
        other => js_to_number(other),
    };
    (time.is_finite() && time.abs() <= MAX_TIME_MILLISECONDS).then(|| iso_string(time))
}

/// `Number(value)`가 0보다 큰 안전한 정수일 때만 남긴다. 값이 없으면 Number(null)처럼 0이다.
fn safe_positive_integer(value: Option<&Value>) -> Option<i64> {
    let number = value.map_or(0.0, js_to_number);
    (number.fract() == 0.0 && number > 0.0 && number <= MAX_SAFE_INTEGER).then_some(number as i64)
}

/// validModelID. `opus[1m]`처럼 컨텍스트 창 접미사가 붙으므로 대괄호도 허용한다.
pub fn valid_model_id(text: &str) -> bool {
    let bytes = text.as_bytes();
    (1..=120).contains(&bytes.len())
        && bytes[0].is_ascii_alphanumeric()
        && bytes[1..].iter().all(|byte| byte.is_ascii_alphanumeric() || b"._:[]-".contains(byte))
}

/// uniqueEfforts. 글자로 바꿔 다듬고 소문자로 만든 뒤 이름 꼴만 남기고, 아는 단계 순서 뒤에 모르는 단계를 둔다.
fn unique_efforts(values: Option<&Value>) -> Vec<String> {
    let mut unique: Vec<String> = Vec::new();
    for value in values.and_then(Value::as_array).into_iter().flatten() {
        let text = js_string_or_empty(Some(value));
        let effort = js_trim(&text).to_lowercase();
        if effort_name(&effort) && !unique.contains(&effort) {
            unique.push(effort);
        }
    }
    unique.sort_by(|left, right| effort_order(left, right));
    unique
}

/// `/^[a-z][a-z0-9_-]{0,31}$/`.
fn effort_name(text: &str) -> bool {
    let bytes = text.as_bytes();
    (1..=32).contains(&bytes.len())
        && bytes[0].is_ascii_lowercase()
        && bytes[1..].iter().all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'_' | b'-'))
}

fn effort_order(left: &str, right: &str) -> std::cmp::Ordering {
    let rank = |effort: &str| EFFORT_ORDER.iter().position(|known| *known == effort);
    match (rank(left), rank(right)) {
        (None, None) => left.bytes().map(collation_weight).cmp(right.bytes().map(collation_weight)),
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (Some(_), None) => std::cmp::Ordering::Less,
        (Some(left), Some(right)) => left.cmp(&right),
    }
}

/// localeCompare(ICU 기본 정렬)에서 단계 이름 글자의 순서. `_` < `-` < 숫자 < 영문 소문자다(Node에서 확인).
fn collation_weight(byte: u8) -> u8 {
    match byte {
        b'_' => 0,
        b'-' => 1,
        b'0'..=b'9' => 2 + (byte - b'0'),
        _ => 12 + byte.saturating_sub(b'a'),
    }
}

/// preferredEffort. high, medium, 첫 단계 순으로 고르고 비었으면 high다.
fn preferred_effort(efforts: &[String]) -> String {
    ["high", "medium"]
        .into_iter()
        .find(|effort| efforts.iter().any(|known| known == effort))
        .map(str::to_string)
        .or_else(|| efforts.first().cloned())
        .unwrap_or_else(|| "high".into())
}

/// `String(source).split(/\r?\n/)`.
fn crlf_lines(source: &str) -> impl Iterator<Item = &str> {
    source.split('\n').map(|line| line.strip_suffix('\r').unwrap_or(line))
}

/// parseCodexModelCatalog. `codex debug models`의 JSON에서 화면에 노출되는(visibility list) 모델만 추린다.
/// 배열이 아닌 값에 map·includes를 부르면 Node처럼 TypeError 문구로 실패한다.
pub fn parse_codex_model_catalog(source: &str) -> Result<ModelCatalog, String> {
    let payload: Value =
        serde_json::from_str(source).map_err(|_| "Codex 모델 카탈로그 JSON을 읽지 못했습니다.".to_string())?;
    let Some(entries) = payload.get("models").and_then(Value::as_array) else {
        return Err("Codex 모델 카탈로그에 models 배열이 없습니다.".into());
    };
    let mut models = Vec::new();
    for entry in entries.iter().filter(|entry| entry.get("visibility") == Some(&json!("list"))) {
        let levels: Vec<Value> = match entry.get("supported_reasoning_levels") {
            None | Some(Value::Null) => Vec::new(),
            Some(Value::Array(levels)) => {
                levels.iter().map(|level| level.get("effort").cloned().unwrap_or(Value::Null)).collect()
            }
            Some(_) => return Err("entry.supported_reasoning_levels?.map is not a function".into()),
        };
        let efforts = unique_efforts(Some(&Value::Array(levels)));
        let default_effort = entry
            .get("default_reasoning_level")
            .and_then(Value::as_str)
            .filter(|level| efforts.iter().any(|effort| effort == level))
            .map_or_else(|| preferred_effort(&efforts), str::to_string);
        let fast = match entry.get("additional_speed_tiers") {
            None | Some(Value::Null) => false,
            Some(Value::Array(tiers)) => tiers.contains(&json!("fast")),
            Some(Value::String(text)) => text.contains("fast"),
            Some(_) => return Err("entry.additional_speed_tiers?.includes is not a function".into()),
        };
        models.extend(normalized_model(&json!({
            "id": entry.get("slug"),
            "title": entry.get("display_name"),
            "efforts": efforts,
            "defaultEffort": default_effort,
            "supportsFastMode": fast,
            "contextWindow": safe_positive_integer(entry.get("context_window")),
            "maxContextWindow": safe_positive_integer(entry.get("max_context_window")),
        })));
    }
    if models.is_empty() {
        return Err("Codex에서 선택 가능한 모델을 찾지 못했습니다.".into());
    }
    Ok(ModelCatalog::new(models))
}

struct EffortVariant {
    base_id: String,
    base_title: String,
    effort: String,
}

/// JS 정규식 `.`(줄 끝 문자 제외)와 `\s`(JS 공백 전체)를 그대로 옮겼다. 대소문자 무시는 /i처럼 ASCII만이다.
static ID_VARIANT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^([^\n\r\x{2028}\x{2029}]*)-(low|medium|high)$").expect("ID 변형 정규식"));
static TITLE_VARIANT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(concat!(
        r"^([^\n\r\x{2028}\x{2029}]*)",
        r"[\t\n\x0B\x0C\r \x{A0}\x{1680}\x{2000}-\x{200A}\x{2028}\x{2029}\x{202F}\x{205F}\x{3000}\x{FEFF}]+",
        r"\(((?i-u:low|medium|high))\)$",
    ))
    .expect("제목 변형 정규식")
});
static CLAUDE_TITLE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^claude-([a-z]+)-([0-9]+)(?:-([0-9]+))?(?:-[0-9]{8})?$").expect("Claude 제목 정규식")
});

fn antigravity_effort_variant(id: &str, title: &str) -> Option<EffortVariant> {
    let identifier = ID_VARIANT.captures(id)?;
    let display = TITLE_VARIANT.captures(title)?;
    let effort = identifier[2].to_lowercase();
    if display[2].to_lowercase() != effort {
        return None;
    }
    Some(EffortVariant { base_id: identifier[1].to_string(), base_title: js_trim(&display[1]).to_string(), effort })
}

/// parseAntigravityModelCatalog. `agy models`의 탭 구분 줄을 읽고, Gemini이거나 변형이 둘 이상인 노력 변형은 한 모델로 합친다.
pub fn parse_antigravity_model_catalog(source: &str) -> Result<ModelCatalog, String> {
    struct Row {
        id: String,
        title: String,
        variant: Option<EffortVariant>,
    }
    let rows: Vec<Row> = crlf_lines(source)
        .filter_map(|line| {
            let mut parts = js_trim(line).split('\t');
            let id = parts.next().unwrap_or("").to_string();
            let title = js_trim(&parts.collect::<Vec<_>>().join(" ")).to_string();
            if !valid_model_id(&id) || title.is_empty() {
                return None;
            }
            let variant = antigravity_effort_variant(&id, &title);
            Some(Row { id, title, variant })
        })
        .collect();
    if rows.is_empty() {
        return Err("Antigravity에서 선택 가능한 모델을 찾지 못했습니다.".into());
    }
    let mut variant_counts: HashMap<&str, usize> = HashMap::new();
    for variant in rows.iter().filter_map(|row| row.variant.as_ref()) {
        *variant_counts.entry(variant.base_id.as_str()).or_default() += 1;
    }
    let mut collected: Vec<CatalogModel> = Vec::new();
    for row in &rows {
        let grouped = row.variant.as_ref().filter(|variant| {
            variant.base_id.starts_with("gemini-") || variant_counts.get(variant.base_id.as_str()).copied().unwrap_or(0) > 1
        });
        let id = grouped.map_or(&row.id, |variant| &variant.base_id);
        let effort = row.variant.as_ref().map_or("default", |variant| variant.effort.as_str());
        if let Some(current) = collected.iter_mut().find(|entry| &entry.id == id) {
            let mut efforts = current.efforts.clone();
            efforts.push(effort.to_string());
            current.efforts = unique_efforts(Some(&json!(efforts)));
            current.default_effort = preferred_effort(&current.efforts);
            continue;
        }
        let title = grouped.map_or(&row.title, |variant| &variant.base_title);
        collected.extend(normalized_model(&json!({
            "id": id, "title": title, "efforts": [effort], "defaultEffort": effort, "supportsFastMode": false,
        })));
    }
    if collected.is_empty() {
        return Err("Antigravity 모델 목록 형식이 올바르지 않습니다.".into());
    }
    Ok(ModelCatalog::new(collected))
}

/// parseClaudeModelCatalog. stream-json 출력에서 initialize control_response의 models를 읽는다.
/// `default`는 다른 항목의 별칭이라 빼고, 비활성 모델과 추론 단계가 없는 모델도 뺀다.
pub fn parse_claude_model_catalog(source: &str) -> Result<ModelCatalog, String> {
    let mut entries: Option<Vec<Value>> = None;
    for line in crlf_lines(source) {
        let Ok(object) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if object.get("type") != Some(&json!("control_response")) {
            continue;
        }
        let response = object.get("response");
        if response.and_then(|response| response.get("subtype")) != Some(&json!("success")) {
            let reason = match response.and_then(|response| response.get("error")) {
                None | Some(Value::Null) => "원인 미상".to_string(),
                Some(reason) => js_string(reason),
            };
            return Err(format!("Claude Code initialize 요청이 실패했습니다: {reason}"));
        }
        if let Some(models) = response
            .and_then(|response| response.get("response"))
            .and_then(|inner| inner.get("models"))
            .and_then(Value::as_array)
        {
            entries = Some(models.clone());
            break;
        }
    }
    let Some(entries) = entries else {
        return Err("Claude Code 응답에 models 배열이 없습니다.".into());
    };
    let models: Vec<CatalogModel> = entries
        .iter()
        .filter_map(|entry| {
            let fields = entry.as_object()?;
            if fields.get("value") == Some(&json!("default")) || fields.get("disabled") == Some(&Value::Bool(true)) {
                return None;
            }
            normalized_model(&json!({
                "id": fields.get("value"),
                "title": claude_model_title(fields.get("displayName"), fields.get("resolvedModel")),
                "efforts": fields.get("supportedEffortLevels"),
                "defaultEffort": "high",
                "supportsFastMode": fields.get("supportsFastMode") == Some(&Value::Bool(true)),
                "resolvedModel": fields.get("resolvedModel"),
            }))
        })
        .collect();
    if models.is_empty() {
        return Err("Claude Code에서 선택 가능한 모델을 찾지 못했습니다.".into());
    }
    Ok(ModelCatalog::new(models))
}

/// claudeModelTitle. 표시명("Fable")에는 버전이 없어 별칭이 가리키는 실제 모델(claude-fable-5-1[1m])에서
/// "Fable 5.1 (1M)"을 만든다. 형식을 모르는 값이면 표시명 뒤에 실제 모델을 붙인다.
pub fn claude_model_title(display_name: Option<&Value>, resolved_model: Option<&Value>) -> String {
    let display_text = js_string_or_empty(display_name);
    let display = js_trim(&display_text);
    let resolved_text = js_string_or_empty(resolved_model);
    let resolved = js_trim(&resolved_text);
    if resolved.is_empty() {
        return display.to_string();
    }
    let one_million = resolved.ends_with("[1m]");
    let bare = if one_million { &resolved[..resolved.len() - 4] } else { resolved };
    let Some(parts) = CLAUDE_TITLE.captures(bare) else {
        return if display.is_empty() { resolved.to_string() } else { format!("{display} · {resolved}") };
    };
    let family = &parts[1];
    let family = format!("{}{}", family[..1].to_uppercase(), &family[1..]);
    let version = match parts.get(3) {
        Some(minor) => format!("{}.{}", &parts[2], minor.as_str()),
        None => parts[2].to_string(),
    };
    format!("{family} {version}{}", if one_million { " (1M)" } else { "" })
}

/// mergeModelCatalog. 같은 식별자가 가리키는 실제 모델이 바뀌면 이전 값과 시각을 남기고, 바뀌지 않으면 지난 기록을 잇는다.
/// 새 목록에서 사라진 모델은 기존 설정 검증용으로 available false로 뒤에 남긴다.
pub fn merge_model_catalog(previous: &ModelCatalog, discovered: &ModelCatalog, now: f64) -> Result<ModelCatalog, String> {
    let old = previous.deduplicated();
    let new = discovered.deduplicated();
    if new.models.is_empty() {
        return Err("새 모델 카탈로그가 비어 있습니다.".into());
    }
    let previous_by_id: HashMap<&str, &CatalogModel> = old.models.iter().map(|entry| (entry.id.as_str(), entry)).collect();
    let discovered_ids: HashSet<&str> = new.models.iter().map(|entry| entry.id.as_str()).collect();
    let mut models: Vec<CatalogModel> = new
        .models
        .iter()
        .map(|entry| {
            let prior = previous_by_id.get(entry.id.as_str()).copied();
            let revised = match (prior.and_then(|prior| prior.resolved_model.as_deref()), entry.resolved_model.as_deref()) {
                (Some(before), Some(after)) => before != after,
                _ => false,
            };
            CatalogModel {
                available: true,
                previous_resolved_model: if revised {
                    prior.and_then(|prior| prior.resolved_model.clone())
                } else {
                    prior.and_then(|prior| prior.previous_resolved_model.clone())
                },
                resolved_model_changed_at: if revised {
                    Some(iso_string(now))
                } else {
                    prior.and_then(|prior| prior.resolved_model_changed_at.clone())
                },
                ..entry.clone()
            }
        })
        .collect();
    models.extend(
        old.models
            .iter()
            .filter(|entry| !discovered_ids.contains(entry.id.as_str()))
            .map(|entry| CatalogModel { available: false, ..entry.clone() }),
    );
    Ok(ModelCatalog::new(models))
}

/// agent_model_catalogs 한 줄. 시각은 node-pg Date처럼 밀리초(내림)다.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ModelCatalogRow {
    pub provider: String,
    pub catalog: Value,
    /// text[]의 NULL 요소는 None이다. Node는 String(null)로 "null"이라 읽는다.
    pub excluded_models: Vec<Option<String>>,
    pub fetched_at: Option<f64>,
    pub last_attempted_at: Option<f64>,
    pub last_error: Option<String>,
}

#[derive(Clone, Copy, Debug)]
pub struct ModelCatalogClaim<'a> {
    pub provider: &'a str,
    pub attempted_at: f64,
    pub minimum_interval_milliseconds: f64,
    pub force: bool,
}

/// createPostgresModelCatalogStore와 같은 연산. 테스트는 메모리 저장소를 쓴다.
pub trait ModelCatalogStore: Send + Sync + 'static {
    fn load(&self) -> BoxFuture<'_, Result<Vec<ModelCatalogRow>, String>>;
    /// 주기가 지났거나 force일 때만 시도 시각을 적고 그 줄을 돌려준다. 아니면 None이다.
    fn claim_refresh<'a>(&'a self, claim: ModelCatalogClaim<'a>) -> BoxFuture<'a, Result<Option<ModelCatalogRow>, String>>;
    fn save_success<'a>(
        &'a self,
        provider: &'a str,
        catalog: &'a ModelCatalog,
        fetched_at: f64,
    ) -> BoxFuture<'a, Result<Option<ModelCatalogRow>, String>>;
    fn save_failure<'a>(&'a self, provider: &'a str, error: &'a str) -> BoxFuture<'a, Result<Option<ModelCatalogRow>, String>>;
    /// 줄이 없으면 fallback_catalog로 새로 만들고, 있으면 제외 목록만 바꾼다.
    fn save_exclusions<'a>(
        &'a self,
        provider: &'a str,
        excluded_models: &'a [String],
        fallback_catalog: &'a ModelCatalog,
    ) -> BoxFuture<'a, Result<Option<ModelCatalogRow>, String>>;
}

pub struct PostgresModelCatalogStore {
    pool: PgPool,
}

impl PostgresModelCatalogStore {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

macro_rules! model_catalog_row_columns {
    () => {
        "provider, catalog, excluded_models, \
         floor(extract(epoch FROM fetched_at) * 1000)::float8, \
         floor(extract(epoch FROM last_attempted_at) * 1000)::float8, \
         last_error"
    };
}

type ModelCatalogRowTuple = (String, Value, Vec<Option<String>>, Option<f64>, Option<f64>, Option<String>);

fn model_catalog_row(
    (provider, catalog, excluded_models, fetched_at, last_attempted_at, last_error): ModelCatalogRowTuple,
) -> ModelCatalogRow {
    ModelCatalogRow { provider, catalog, excluded_models, fetched_at, last_attempted_at, last_error }
}

fn store_error(error: sqlx::Error) -> String {
    crate::rag::database_message(&error)
}

/// JSON.stringify(catalog). jsonb로 저장하므로 키 순서는 읽을 때 다시 정리된다.
fn catalog_text(catalog: &ModelCatalog) -> Result<String, String> {
    String::from_utf8(to_js_json(catalog).map_err(|error| error.to_string())?).map_err(|error| error.to_string())
}

impl ModelCatalogStore for PostgresModelCatalogStore {
    fn load(&self) -> BoxFuture<'_, Result<Vec<ModelCatalogRow>, String>> {
        Box::pin(async move {
            let rows: Vec<ModelCatalogRowTuple> =
                sqlx::query_as(concat!("SELECT ", model_catalog_row_columns!(), " FROM agent_model_catalogs"))
                    .fetch_all(&self.pool)
                    .await
                    .map_err(store_error)?;
            Ok(rows.into_iter().map(model_catalog_row).collect())
        })
    }

    fn claim_refresh<'a>(&'a self, claim: ModelCatalogClaim<'a>) -> BoxFuture<'a, Result<Option<ModelCatalogRow>, String>> {
        Box::pin(async move {
            let row: Option<ModelCatalogRowTuple> = sqlx::query_as(concat!(
                "INSERT INTO agent_model_catalogs (provider, last_attempted_at, updated_at) ",
                "VALUES ($1, $2::timestamptz, now()) ",
                "ON CONFLICT (provider) DO UPDATE SET last_attempted_at = EXCLUDED.last_attempted_at, updated_at = now() ",
                "WHERE $4::boolean OR agent_model_catalogs.last_attempted_at IS NULL ",
                "OR agent_model_catalogs.last_attempted_at <= $2::timestamptz - ($3::double precision * interval '1 millisecond') ",
                "RETURNING ",
                model_catalog_row_columns!()
            ))
            .bind(claim.provider)
            .bind(iso_string(claim.attempted_at))
            .bind(claim.minimum_interval_milliseconds)
            .bind(claim.force)
            .fetch_optional(&self.pool)
            .await
            .map_err(store_error)?;
            Ok(row.map(model_catalog_row))
        })
    }

    fn save_success<'a>(
        &'a self,
        provider: &'a str,
        catalog: &'a ModelCatalog,
        fetched_at: f64,
    ) -> BoxFuture<'a, Result<Option<ModelCatalogRow>, String>> {
        Box::pin(async move {
            let row: Option<ModelCatalogRowTuple> = sqlx::query_as(concat!(
                "UPDATE agent_model_catalogs SET catalog = $2::jsonb, fetched_at = $3::timestamptz, ",
                "last_error = NULL, updated_at = now() WHERE provider = $1 RETURNING ",
                model_catalog_row_columns!()
            ))
            .bind(provider)
            .bind(catalog_text(catalog)?)
            .bind(iso_string(fetched_at))
            .fetch_optional(&self.pool)
            .await
            .map_err(store_error)?;
            Ok(row.map(model_catalog_row))
        })
    }

    fn save_failure<'a>(&'a self, provider: &'a str, error: &'a str) -> BoxFuture<'a, Result<Option<ModelCatalogRow>, String>> {
        Box::pin(async move {
            let row: Option<ModelCatalogRowTuple> = sqlx::query_as(concat!(
                "UPDATE agent_model_catalogs SET last_error = $2, updated_at = now() WHERE provider = $1 RETURNING ",
                model_catalog_row_columns!()
            ))
            .bind(provider)
            .bind(utf16_prefix(error, 1_000))
            .fetch_optional(&self.pool)
            .await
            .map_err(store_error)?;
            Ok(row.map(model_catalog_row))
        })
    }

    fn save_exclusions<'a>(
        &'a self,
        provider: &'a str,
        excluded_models: &'a [String],
        fallback_catalog: &'a ModelCatalog,
    ) -> BoxFuture<'a, Result<Option<ModelCatalogRow>, String>> {
        Box::pin(async move {
            let row: Option<ModelCatalogRowTuple> = sqlx::query_as(concat!(
                "INSERT INTO agent_model_catalogs (provider, catalog, excluded_models, updated_at) ",
                "VALUES ($1, $2::jsonb, $3::text[], now()) ",
                "ON CONFLICT (provider) DO UPDATE SET excluded_models = EXCLUDED.excluded_models, updated_at = now() ",
                "RETURNING ",
                model_catalog_row_columns!()
            ))
            .bind(provider)
            .bind(catalog_text(fallback_catalog)?)
            .bind(excluded_models)
            .fetch_optional(&self.pool)
            .await
            .map_err(store_error)?;
            Ok(row.map(model_catalog_row))
        })
    }
}

/// 서비스가 밖에 기대는 일. Node 생성자의 resolveExecutable·runCommand·executableChangedAt이다.
pub trait ModelCatalogHost: Send + Sync + 'static {
    /// 제공자의 CLI 실행 파일. 운영은 최근에 바뀐 직원 설정과 설치 위치에서 찾는다.
    fn resolve_executable<'a>(&'a self, provider: &'a str) -> BoxFuture<'a, Result<String, String>>;
    /// 목록 명령의 표준 출력. 실패 문구는 Node execFile 오류의 message와 같다.
    fn run_command<'a>(
        &'a self,
        executable: &'a str,
        arguments: &'a [&'a str],
        input: Option<&'a str>,
    ) -> BoxFuture<'a, Result<String, String>>;
    /// 심볼릭 링크를 따라간 실제 실행 파일의 ctime(밀리초). 읽지 못하면 None이다.
    fn executable_changed_at<'a>(&'a self, executable: &'a str) -> BoxFuture<'a, Option<f64>>;
}

/// 운영 호스트. Node server.mjs의 configuredModelCatalogExecutable과 model-catalog.mjs의 기본 실행기다.
pub struct SystemModelCatalogHost {
    pool: PgPool,
}

impl SystemModelCatalogHost {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

impl ModelCatalogHost for SystemModelCatalogHost {
    fn resolve_executable<'a>(&'a self, provider: &'a str) -> BoxFuture<'a, Result<String, String>> {
        Box::pin(async move {
            let config: Option<Value> = sqlx::query_scalar(
                "SELECT config FROM characters WHERE backend = $1 ORDER BY updated_at DESC, id LIMIT 1",
            )
            .bind(provider)
            .fetch_optional(&self.pool)
            .await
            .map_err(store_error)?;
            locate_executable(provider, &config.unwrap_or_else(|| json!({})))
        })
    }

    fn run_command<'a>(
        &'a self,
        executable: &'a str,
        arguments: &'a [&'a str],
        input: Option<&'a str>,
    ) -> BoxFuture<'a, Result<String, String>> {
        Box::pin(run_catalog_command(executable, arguments, input, COMMAND_TIMEOUT, MAX_COMMAND_OUTPUT_BYTES))
    }

    fn executable_changed_at<'a>(&'a self, executable: &'a str) -> BoxFuture<'a, Option<f64>> {
        Box::pin(async move {
            let real = tokio::fs::canonicalize(executable).await.ok()?;
            let metadata = tokio::fs::metadata(real).await.ok()?;
            Some(metadata.ctime() as f64 * 1_000.0 + metadata.ctime_nsec() as f64 / 1_000_000.0)
        })
    }
}

/// Node runCatalogCommand. execFile을 그대로 쓰고 표준 출력만 돌려준다.
pub async fn run_catalog_command(
    executable: &str,
    arguments: &[&str],
    input: Option<&str>,
    timeout: Duration,
    max_bytes: usize,
) -> Result<String, String> {
    exec_file(executable, arguments, input, timeout, max_bytes).await.map(|output| output.stdout)
}

/// setExclusions 실패. 검증 오류는 Node ModelCatalogValidationError(400), 저장 실패는 500이다.
#[derive(Clone, Debug, PartialEq)]
pub enum ModelCatalogError {
    Validation(String),
    Failure(String),
}

impl std::fmt::Display for ModelCatalogError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Validation(message) | Self::Failure(message) => formatter.write_str(message),
        }
    }
}

/// 서비스 설정. 기본값은 Date.now, 표준 오류 출력 경고, 알림 없음, 12시간 주기다.
#[derive(Clone)]
pub struct ModelCatalogOptions {
    pub now: Arc<dyn Fn() -> f64 + Send + Sync>,
    pub warn: Arc<dyn Fn(&str, &str) + Send + Sync>,
    /// 목록·제외 설정이 바뀐 제공자를 알린다. Node는 model-catalog.changed를 방송하며, 방송은 웹소켓을 옮길 때 연결한다.
    pub on_changed: Arc<dyn Fn(&str) + Send + Sync>,
    pub refresh_interval_milliseconds: f64,
}

impl Default for ModelCatalogOptions {
    fn default() -> Self {
        Self {
            now: Arc::new(now_milliseconds),
            warn: Arc::new(|message, detail| eprintln!("{message} {detail}")),
            on_changed: Arc::new(|_| {}),
            refresh_interval_milliseconds: MODEL_CATALOG_REFRESH_MILLISECONDS,
        }
    }
}

/// 서비스가 들고 있는 정리된 줄(Node normalizedRow).
#[derive(Clone, Debug, Default)]
struct CatalogRow {
    catalog: ModelCatalog,
    excluded_models: Vec<String>,
    fetched_at: Option<f64>,
    last_attempted_at: Option<f64>,
    last_error: Option<String>,
}

fn normalized_row(row: ModelCatalogRow) -> CatalogRow {
    let excluded: Vec<String> =
        row.excluded_models.into_iter().map(|id| id.unwrap_or_else(|| "null".into())).collect();
    CatalogRow {
        catalog: normalize_catalog(&row.catalog),
        excluded_models: normalized_exclusions(excluded),
        fetched_at: row.fetched_at,
        last_attempted_at: row.last_attempted_at,
        last_error: row.last_error,
    }
}

/// normalizedExclusions. 중복을 빼고 올바른 식별자만 남긴다.
fn normalized_exclusions(values: Vec<String>) -> Vec<String> {
    let mut seen = HashSet::new();
    values.into_iter().filter(|id| seen.insert(id.clone())).filter(|id| valid_model_id(id)).collect()
}

/// catalogFor. 저장된 목록을 쓰되 Antigravity 고정 Thinking 모델은 default 단계로 바로잡고, 비었으면 내장 기본값이다.
fn catalog_from_row(provider: &str, row: Option<&CatalogRow>) -> Result<ModelCatalog, String> {
    let mut stored = row.map(|row| row.catalog.deduplicated()).unwrap_or_default();
    if provider == "antigravity" {
        for entry in &mut stored.models {
            if is_antigravity_fixed_thinking_model(&entry.id) {
                entry.efforts = vec!["default".into()];
                entry.default_effort = "default".into();
            }
        }
    }
    if !stored.models.is_empty() {
        return Ok(stored);
    }
    builtin_model_catalog(provider).ok_or_else(|| format!("지원하지 않는 모델 제공자입니다: {provider}"))
}

/// Node ModelCatalogService. 제공자별로 12시간에 한 번 CLI 목록을 받고, 실패하면 마지막 정상값을 지킨다.
pub struct ModelCatalogService {
    store: Arc<dyn ModelCatalogStore>,
    host: Arc<dyn ModelCatalogHost>,
    options: ModelCatalogOptions,
    rows: Mutex<HashMap<String, CatalogRow>>,
    timer: Mutex<Option<tokio::task::JoinHandle<()>>>,
    running: AtomicBool,
    stopped: AtomicBool,
}

impl ModelCatalogService {
    pub fn new(
        store: Arc<dyn ModelCatalogStore>,
        host: Arc<dyn ModelCatalogHost>,
        options: ModelCatalogOptions,
    ) -> Arc<Self> {
        Arc::new(Self {
            store,
            host,
            options,
            rows: Mutex::new(HashMap::new()),
            timer: Mutex::new(None),
            running: AtomicBool::new(false),
            stopped: AtomicBool::new(true),
        })
    }

    fn row(&self, provider: &str) -> Option<CatalogRow> {
        self.rows.lock().unwrap_or_else(PoisonError::into_inner).get(provider).cloned()
    }

    fn set_row(&self, provider: &str, row: CatalogRow) {
        self.rows.lock().unwrap_or_else(PoisonError::into_inner).insert(provider.to_string(), row);
    }

    /// 저장된 목록을 읽는다. 모르는 제공자 줄은 건너뛴다.
    pub async fn load_cached(&self) -> Result<Value, String> {
        for row in self.store.load().await? {
            if MODEL_CATALOG_PROVIDERS.contains(&row.provider.as_str()) {
                let provider = row.provider.clone();
                self.set_row(&provider, normalized_row(row));
            }
        }
        Ok(self.snapshot())
    }

    pub fn start(self: &Arc<Self>) {
        if !self.stopped.swap(false, Ordering::SeqCst) {
            return;
        }
        let service = Arc::clone(self);
        tokio::spawn(async move { service.run_cycle().await });
    }

    /// 다음 주기 예약을 거둔다. 이미 돌고 있는 갱신은 Node처럼 끝까지 간다.
    pub fn stop(&self) {
        self.stopped.store(true, Ordering::SeqCst);
        if let Some(timer) = self.timer.lock().unwrap_or_else(PoisonError::into_inner).take() {
            timer.abort();
        }
    }

    pub fn catalog_for(&self, provider: &str) -> Result<ModelCatalog, String> {
        catalog_from_row(provider, self.row(provider).as_ref())
    }

    /// modelCapabilities. 내려간(available false) 모델도 기존 설정 검증용으로 찾는다.
    pub fn model_capabilities(&self, provider: &str, model_id: &str) -> Result<Option<CatalogModel>, String> {
        Ok(self.catalog_for(provider)?.models.into_iter().find(|entry| entry.id == model_id))
    }

    /// GET /api/model-catalog 본문. 선택 가능한 모델과 그중 제외한 식별자, 수집 시각을 제공자 순서로 담는다.
    pub fn snapshot(&self) -> Value {
        let rows = self.rows.lock().unwrap_or_else(PoisonError::into_inner);
        let time = |value: Option<f64>| value.map_or(Value::Null, |milliseconds| Value::from(iso_string(milliseconds)));
        let providers: Vec<Value> = MODEL_CATALOG_PROVIDERS
            .iter()
            .map(|provider| {
                let row = rows.get(*provider);
                let models: Vec<CatalogModel> = catalog_from_row(provider, row)
                    .map(|catalog| catalog.models)
                    .unwrap_or_default()
                    .into_iter()
                    .filter(|entry| entry.available)
                    .collect();
                let ids: HashSet<&str> = models.iter().map(|entry| entry.id.as_str()).collect();
                let excluded: Vec<&String> =
                    row.map_or(&[][..], |row| &row.excluded_models).iter().filter(|id| ids.contains(id.as_str())).collect();
                json!({
                    "backend": provider,
                    "models": models,
                    "excludedModels": excluded,
                    "fetchedAt": time(row.and_then(|row| row.fetched_at)),
                    "lastAttemptedAt": time(row.and_then(|row| row.last_attempted_at)),
                    "lastError": row.and_then(|row| row.last_error.clone()),
                })
            })
            .collect();
        json!({ "providers": providers })
    }

    /// 세 제공자에 같은 일을 동시에 시킨다. Promise.all처럼 먼저 실패한 쪽 오류를 돌려주고 남은 일은 그대로 진행된다.
    async fn for_each_provider<F, Fut>(self: &Arc<Self>, task: F) -> Result<Vec<Value>, String>
    where
        F: Fn(Arc<Self>, &'static str) -> Fut,
        Fut: Future<Output = Result<Value, String>> + Send + 'static,
    {
        let mut pending: FuturesUnordered<_> = MODEL_CATALOG_PROVIDERS
            .iter()
            .enumerate()
            .map(|(index, provider)| {
                let handle = tokio::spawn(task(Arc::clone(self), provider));
                async move { (index, handle.await) }
            })
            .collect();
        let mut results = vec![Value::Null; MODEL_CATALOG_PROVIDERS.len()];
        while let Some((index, finished)) = pending.next().await {
            results[index] = finished.map_err(|error| error.to_string())??;
        }
        Ok(results)
    }

    pub async fn refresh_due(self: &Arc<Self>, force: bool) -> Result<Vec<Value>, String> {
        self.for_each_provider(move |service, provider| async move { service.refresh_provider(provider, force).await })
            .await
    }

    /// CLI가 마지막 수집 시도 뒤에 설치됐으면 12시간을 기다리지 않고 다시 받는다. 저장된 시도 시각과 비교하므로
    /// 백엔드가 꺼진 동안의 갱신도 잡힌다. 화이트보드가 CLI 업데이트 상태를 조회할 때 함께 부른다.
    pub async fn refresh_updated_executables(self: &Arc<Self>) -> Result<Vec<Value>, String> {
        self.for_each_provider(|service, provider| async move {
            let attempted = service.row(provider).and_then(|row| row.last_attempted_at);
            let executable = service.host.resolve_executable(provider).await?;
            let changed_at = service.host.executable_changed_at(&executable).await;
            match (attempted, changed_at) {
                (Some(attempted), Some(changed_at))
                    if attempted.is_finite() && changed_at.is_finite() && changed_at > attempted =>
                {
                    service.refresh_provider(provider, true).await
                }
                _ => Ok(json!({ "provider": provider, "status": "fresh" })),
            }
        })
        .await
    }

    pub async fn refresh_provider(&self, provider: &str, force: bool) -> Result<Value, String> {
        let Some(source) = catalog_source(provider) else {
            return Err(format!("지원하지 않는 모델 제공자입니다: {provider}"));
        };
        let claim = ModelCatalogClaim {
            provider,
            attempted_at: (self.options.now)(),
            minimum_interval_milliseconds: self.options.refresh_interval_milliseconds,
            force,
        };
        let Some(claimed) = self.store.claim_refresh(claim).await? else {
            return Ok(json!({ "provider": provider, "status": "fresh" }));
        };
        let claimed = normalized_row(claimed);
        self.set_row(provider, claimed.clone());
        match self.refresh_claimed(source, &claimed).await {
            Ok(model_count) => Ok(json!({ "provider": provider, "status": "updated", "modelCount": model_count })),
            Err(message) => {
                let saved = self.store.save_failure(provider, &message).await?;
                let row = saved.map_or_else(|| CatalogRow { last_error: Some(message.clone()), ..claimed }, normalized_row);
                self.set_row(provider, row);
                (self.options.warn)(&format!("{provider} 모델 목록을 갱신하지 못해 마지막 정상값을 유지합니다."), &message);
                Ok(json!({ "provider": provider, "status": "failed", "error": message }))
            }
        }
    }

    /// 받은 목록을 지금 목록과 합쳐 저장한다. Node try 블록과 같은 범위라 저장 실패도 갱신 실패로 남는다.
    async fn refresh_claimed(&self, source: &CatalogSource, claimed: &CatalogRow) -> Result<usize, String> {
        let discovered = self.fetch_provider(source).await?;
        let merged = merge_model_catalog(&self.catalog_for(source.provider)?, &discovered, (self.options.now)())?;
        let fetched_at = (self.options.now)();
        let saved = self.store.save_success(source.provider, &merged, fetched_at).await?;
        let row = saved.map_or_else(
            || CatalogRow {
                catalog: merged.deduplicated(),
                fetched_at: Some(fetched_at),
                last_error: None,
                ..claimed.clone()
            },
            normalized_row,
        );
        self.set_row(source.provider, row);
        (self.options.on_changed)(source.provider);
        Ok(merged.models.iter().filter(|entry| entry.available).count())
    }

    async fn fetch_provider(&self, source: &CatalogSource) -> Result<ModelCatalog, String> {
        let executable = self.host.resolve_executable(source.provider).await?;
        let stdout = self.host.run_command(&executable, source.arguments, source.input).await?;
        if stdout.len() > MAX_COMMAND_OUTPUT_BYTES {
            return Err(format!("{} 모델 목록이 허용 크기를 넘었습니다.", source.provider));
        }
        (source.parse)(&stdout)
    }

    /// setExclusions. 선택 목록에 있는 모델만 제외하고, 제공자마다 하나는 남겨야 한다.
    pub async fn set_exclusions(&self, provider: &str, values: Option<&Value>) -> Result<Value, ModelCatalogError> {
        if !MODEL_CATALOG_PROVIDERS.contains(&provider) {
            return Err(ModelCatalogError::Validation("지원하지 않는 모델 제공자입니다.".into()));
        }
        let Some(Value::Array(values)) = values else {
            return Err(ModelCatalogError::Validation("제외 모델은 배열이어야 합니다.".into()));
        };
        let mut seen = HashSet::new();
        let requested: Vec<String> = values
            .iter()
            .map(|value| js_string_or_empty(Some(value)))
            .filter(|id| seen.insert(id.clone()))
            .filter(|id| !id.is_empty())
            .collect();
        if requested.iter().any(|id| !valid_model_id(id)) {
            return Err(ModelCatalogError::Validation("제외 모델 식별자가 올바르지 않습니다.".into()));
        }
        let catalog = self.catalog_for(provider).map_err(ModelCatalogError::Failure)?;
        let available_ids: HashSet<&str> =
            catalog.models.iter().filter(|entry| entry.available).map(|entry| entry.id.as_str()).collect();
        let exclusions: Vec<String> = requested.into_iter().filter(|id| available_ids.contains(id.as_str())).collect();
        if !available_ids.is_empty() && exclusions.len() >= available_ids.len() {
            return Err(ModelCatalogError::Validation("각 CLI에는 표시할 모델을 하나 이상 남겨야 합니다.".into()));
        }
        let saved =
            self.store.save_exclusions(provider, &exclusions, &catalog).await.map_err(ModelCatalogError::Failure)?;
        let row = match saved {
            Some(saved) => normalized_row(saved),
            None => CatalogRow {
                catalog: self.catalog_for(provider).map_err(ModelCatalogError::Failure)?,
                excluded_models: normalized_exclusions(exclusions),
                ..self.row(provider).unwrap_or_default()
            },
        };
        self.set_row(provider, row);
        (self.options.on_changed)(provider);
        Ok(self.snapshot())
    }

    async fn run_cycle(self: Arc<Self>) {
        if self.stopped.load(Ordering::SeqCst) || self.running.swap(true, Ordering::SeqCst) {
            return;
        }
        if let Err(message) = self.refresh_due(false).await {
            (self.options.warn)("CLI 모델 목록 갱신 주기를 완료하지 못했습니다.", &message);
        }
        self.running.store(false, Ordering::SeqCst);
        self.schedule_next_cycle();
    }

    fn schedule_next_cycle(self: &Arc<Self>) {
        if self.stopped.load(Ordering::SeqCst) {
            return;
        }
        let mut timer = self.timer.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(previous) = timer.take() {
            previous.abort();
        }
        let delay = self.next_cycle_delay();
        let service = Arc::clone(self);
        *timer = Some(tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs_f64(delay / 1_000.0)).await;
            service.timer.lock().unwrap_or_else(PoisonError::into_inner).take();
            service.run_cycle().await;
        }));
    }

    /// 가장 이른 제공자의 마지막 시도 + 주기까지 남은 밀리초. 시도 기록이 없으면 지금이고 최소 1초다.
    /// 가격표와 달리 Date.getTime을 쓰므로 밀리초를 버리지 않는다.
    fn next_cycle_delay(&self) -> f64 {
        let now = (self.options.now)();
        let interval = self.options.refresh_interval_milliseconds;
        let rows = self.rows.lock().unwrap_or_else(PoisonError::into_inner);
        let next = MODEL_CATALOG_PROVIDERS
            .iter()
            .map(|provider| match rows.get(*provider).and_then(|row| row.last_attempted_at) {
                Some(attempted) if attempted.is_finite() => attempted + interval,
                _ => now,
            })
            .fold(f64::INFINITY, f64::min);
        js_max(1_000.0, next - now)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ANTIGRAVITY_PAYLOAD: &str = "gemini-next-high\tGemini Next (High)\n\
        gemini-next-medium\tGemini Next (Medium)\n\
        gemini-next-low\tGemini Next (Low)\n\
        claude-sonnet-4-6\tClaude Sonnet 4.6 (Thinking)\n\
        claude-opus-4-6-thinking\tClaude Opus 4.6 (Thinking)\n\
        gpt-oss-120b-medium\tGPT-OSS 120B (Medium)";

    fn codex_payload() -> String {
        json!({ "models": [
            { "slug": "internal-hidden", "display_name": "Hidden", "visibility": "hide", "default_reasoning_level": "high",
              "supported_reasoning_levels": [{ "effort": "high" }] },
            { "slug": "gpt-next", "display_name": "GPT Next", "visibility": "list", "default_reasoning_level": "medium",
              "supported_reasoning_levels": [{ "effort": "low" }, { "effort": "medium" }, { "effort": "high" }],
              "additional_speed_tiers": ["fast"], "service_tiers": [{ "id": "priority" }],
              "context_window": 300_000, "max_context_window": 1_000_000 },
        ] })
        .to_string()
    }

    /// stream-json은 initialize control_response 앞뒤로 다른 줄을 낼 수 있다.
    fn claude_payload() -> String {
        let models = json!([
            { "value": "default", "resolvedModel": "claude-opus-5[1m]", "displayName": "Default (recommended)",
              "supportsEffort": true, "supportedEffortLevels": ["low", "medium", "high", "xhigh", "max"], "supportsFastMode": true },
            { "value": "opus[1m]", "resolvedModel": "claude-opus-5[1m]", "displayName": "Opus (1M context)",
              "supportsEffort": true, "supportedEffortLevels": ["max", "low", "medium", "high", "xhigh"], "supportsFastMode": true },
            { "value": "fable[1m]", "resolvedModel": "claude-fable-5-1[1m]", "displayName": "Fable",
              "supportsEffort": true, "supportedEffortLevels": ["low", "medium", "high", "xhigh", "max"] },
            { "value": "haiku", "displayName": "Haiku" },
            { "value": "restricted", "displayName": "Restricted", "disabled": true, "supportedEffortLevels": ["high"] },
        ]);
        [
            json!({ "type": "system", "subtype": "init", "session_id": "s" }).to_string(),
            json!({ "type": "control_response", "response": {
                "subtype": "success", "request_id": "officestra-model-catalog", "response": { "models": models },
            } })
            .to_string(),
        ]
        .join("\n")
    }

    fn ids(catalog: &ModelCatalog) -> Vec<&str> {
        catalog.models.iter().map(|entry| entry.id.as_str()).collect()
    }

    fn statuses(results: &[Value]) -> Vec<&str> {
        results.iter().map(|entry| entry["status"].as_str().unwrap()).collect()
    }

    fn time(text: &str) -> f64 {
        date_parse(text).unwrap()
    }

    #[test]
    fn claude_initialize_reads_selector_values_and_skips_alias_plain_and_disabled() {
        let parsed = parse_claude_model_catalog(&claude_payload()).unwrap();
        assert_eq!(ids(&parsed), ["opus[1m]", "fable[1m]"]);
        assert_eq!(
            parsed.models[0].clone(),
            CatalogModel {
                id: "opus[1m]".into(),
                title: "Opus 5 (1M)".into(),
                efforts: ["low", "medium", "high", "xhigh", "max"].map(String::from).to_vec(),
                default_effort: "high".into(),
                supports_fast_mode: true,
                context_window: None,
                max_context_window: None,
                available: true,
                resolved_model: Some("claude-opus-5[1m]".into()),
                previous_resolved_model: None,
                resolved_model_changed_at: None,
            }
        );
        assert_eq!(
            to_js_json(&parsed.models[0]).unwrap(),
            br#"{"id":"opus[1m]","title":"Opus 5 (1M)","efforts":["low","medium","high","xhigh","max"],"defaultEffort":"high","supportsFastMode":true,"contextWindow":null,"maxContextWindow":null,"available":true,"resolvedModel":"claude-opus-5[1m]","previousResolvedModel":null,"resolvedModelChangedAt":null}"#
        );
        assert_eq!(parsed.models[1].title, "Fable 5.1 (1M)");
        assert!(!parsed.models[1].supports_fast_mode);
        let failure = json!({ "type": "control_response", "response": { "subtype": "error", "error": "not logged in" } });
        assert_eq!(
            parse_claude_model_catalog(&failure.to_string()),
            Err("Claude Code initialize 요청이 실패했습니다: not logged in".into())
        );
        assert_eq!(parse_claude_model_catalog(""), Err("Claude Code 응답에 models 배열이 없습니다.".into()));
    }

    #[test]
    fn codex_json_keeps_listed_models_and_real_options() {
        let parsed = parse_codex_model_catalog(&codex_payload()).unwrap();
        assert_eq!(parsed.models.len(), 1);
        assert_eq!(
            parsed.models[0].to_owned(),
            CatalogModel {
                id: "gpt-next".into(),
                title: "GPT Next".into(),
                efforts: ["low", "medium", "high"].map(String::from).to_vec(),
                default_effort: "medium".into(),
                supports_fast_mode: true,
                context_window: Some(300_000),
                max_context_window: Some(1_000_000),
                available: true,
                resolved_model: None,
                previous_resolved_model: None,
                resolved_model_changed_at: None,
            }
        );
    }

    #[test]
    fn codex_type_errors_follow_node_messages() {
        let entry = |extra: Value| {
            let mut entry = json!({ "slug": "x", "visibility": "list", "supported_reasoning_levels": [{ "effort": "high" }] });
            entry.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
            json!({ "models": [entry] }).to_string()
        };
        assert_eq!(
            parse_codex_model_catalog(&entry(json!({ "supported_reasoning_levels": "high" }))),
            Err("entry.supported_reasoning_levels?.map is not a function".into())
        );
        assert_eq!(
            parse_codex_model_catalog(&entry(json!({ "additional_speed_tiers": 1 }))),
            Err("entry.additional_speed_tiers?.includes is not a function".into())
        );
        let text_tiers = parse_codex_model_catalog(&entry(json!({ "additional_speed_tiers": "fast,flex" }))).unwrap();
        assert!(text_tiers.models[0].supports_fast_mode);
        assert_eq!(parse_codex_model_catalog("{"), Err("Codex 모델 카탈로그 JSON을 읽지 못했습니다.".into()));
        assert_eq!(parse_codex_model_catalog("[]"), Err("Codex 모델 카탈로그에 models 배열이 없습니다.".into()));
        assert_eq!(
            parse_codex_model_catalog(r#"{"models":[{"slug":"-bad","visibility":"list"}]}"#),
            Err("Codex에서 선택 가능한 모델을 찾지 못했습니다.".into())
        );
    }

    #[test]
    fn claude_title_shows_resolved_model_version() {
        let title = |display: Value, resolved: Value| claude_model_title(Some(&display), Some(&resolved));
        assert_eq!(title(json!("Fable"), json!("claude-fable-5-1[1m]")), "Fable 5.1 (1M)");
        assert_eq!(title(json!("Sonnet"), json!("claude-sonnet-5")), "Sonnet 5");
        assert_eq!(title(json!("Haiku"), json!("claude-haiku-4-5-20251001")), "Haiku 4.5");
        assert_eq!(title(json!("Custom"), json!("my-gateway-model")), "Custom · my-gateway-model");
        assert_eq!(title(json!("Opus"), Value::Null), "Opus");
    }

    /// 같은 별칭(fable[1m])이 새 모델을 가리키면 이전 값과 시각을 남기고, 다음 갱신에서 변화가 없으면 그 기록을 잇는다.
    #[test]
    fn merge_keeps_previous_resolved_model_and_time() {
        let payload = claude_payload();
        let first =
            merge_model_catalog(&ModelCatalog::default(), &parse_claude_model_catalog(&payload).unwrap(), time("2026-09-05T00:00:00Z"))
                .unwrap();
        assert_eq!(first.models[1].resolved_model_changed_at, None);
        let revised = payload.replace("claude-fable-5-1[1m]", "claude-fable-5-2[1m]");
        let second =
            merge_model_catalog(&first, &parse_claude_model_catalog(&revised).unwrap(), time("2026-10-01T00:00:00Z")).unwrap();
        let fable = second.models.iter().find(|entry| entry.id == "fable[1m]").unwrap();
        assert_eq!(fable.title, "Fable 5.2 (1M)");
        assert_eq!(fable.resolved_model.as_deref(), Some("claude-fable-5-2[1m]"));
        assert_eq!(fable.previous_resolved_model.as_deref(), Some("claude-fable-5-1[1m]"));
        assert_eq!(fable.resolved_model_changed_at.as_deref(), Some("2026-10-01T00:00:00.000Z"));
        assert_eq!(second.models[0].previous_resolved_model, None);
        let third =
            merge_model_catalog(&second, &parse_claude_model_catalog(&revised).unwrap(), time("2026-10-02T00:00:00Z")).unwrap();
        let carried = third.models.iter().find(|entry| entry.id == "fable[1m]").unwrap();
        assert_eq!(carried.previous_resolved_model.as_deref(), Some("claude-fable-5-1[1m]"));
        assert_eq!(carried.resolved_model_changed_at.as_deref(), Some("2026-10-01T00:00:00.000Z"));
    }

    #[test]
    fn antigravity_groups_gemini_effort_variants() {
        let parsed = parse_antigravity_model_catalog(ANTIGRAVITY_PAYLOAD).unwrap();
        assert_eq!(ids(&parsed), ["gemini-next", "claude-sonnet-4-6", "claude-opus-4-6-thinking", "gpt-oss-120b-medium"]);
        assert_eq!(parsed.models[0].efforts, ["low", "medium", "high"]);
        assert_eq!(parsed.models[0].title, "Gemini Next");
        assert_eq!(parsed.models[1].default_effort, "default");
        assert_eq!(parsed.models[2].efforts, ["default"]);
        assert_eq!(parsed.models[3].efforts, ["medium"]);
    }

    #[test]
    fn model_normalization_follows_js_coercions() {
        let model = normalized_model(&json!({
            "id": ["gpt", "x"], "title": "  ", "efforts": [" HIGH ", "zeta", "a-b", "a_9", "a0", null, "low", "high", "Ab!"],
            "defaultEffort": "LOW", "supportsFastMode": "true", "contextWindow": true, "maxContextWindow": "0x10",
            "available": null, "resolvedModel": 5, "previousResolvedModel": [], "resolvedModelChangedAt": true,
        }));
        assert_eq!(model, None, "쉼표가 들어간 String(['gpt','x'])는 식별자가 아니다");
        let model = normalized_model(&json!({
            "id": ["gpt"], "title": "  ", "efforts": [" HIGH ", "zeta", "a-b", "a_9", "a0", null, "low", "high", "Ab!"],
            "defaultEffort": "LOW", "supportsFastMode": "true", "contextWindow": true, "maxContextWindow": "0x10",
            "available": null, "resolvedModel": 5, "previousResolvedModel": [], "resolvedModelChangedAt": true,
        }))
        .unwrap();
        assert_eq!(model.id, "gpt");
        assert_eq!(model.title, "gpt");
        assert_eq!(model.efforts, ["low", "high", "a_9", "a-b", "a0", "zeta"]);
        assert_eq!(model.default_effort, "high");
        assert!(!model.supports_fast_mode);
        assert_eq!((model.context_window, model.max_context_window), (Some(1), Some(16)));
        assert!(model.available);
        assert_eq!(model.resolved_model.as_deref(), Some("5"));
        assert_eq!(model.previous_resolved_model, None);
        assert_eq!(model.resolved_model_changed_at.as_deref(), Some("1970-01-01T00:00:00.001Z"));
        assert_eq!(normalized_model(&json!({ "id": "x", "efforts": "high" })), None);
        assert_eq!(normalized_model(&json!("x")), None);
    }

    #[derive(Default)]
    struct FakeStore {
        rows: Mutex<HashMap<String, ModelCatalogRow>>,
    }

    impl FakeStore {
        fn get(&self, provider: &str) -> Option<ModelCatalogRow> {
            self.rows.lock().unwrap().get(provider).cloned()
        }

        fn put(&self, row: ModelCatalogRow) -> Option<ModelCatalogRow> {
            self.rows.lock().unwrap().insert(row.provider.clone(), row.clone());
            Some(row)
        }
    }

    /// Node 테스트의 fakeStore와 같은 규칙.
    impl ModelCatalogStore for FakeStore {
        fn load(&self) -> BoxFuture<'_, Result<Vec<ModelCatalogRow>, String>> {
            let rows = self.rows.lock().unwrap().values().cloned().collect();
            Box::pin(async move { Ok(rows) })
        }

        fn claim_refresh<'a>(&'a self, claim: ModelCatalogClaim<'a>) -> BoxFuture<'a, Result<Option<ModelCatalogRow>, String>> {
            let previous = self.get(claim.provider);
            let recent = previous.as_ref().and_then(|row| row.last_attempted_at).is_some_and(|attempted| {
                claim.attempted_at - attempted < claim.minimum_interval_milliseconds
            });
            let result = if !claim.force && recent {
                None
            } else {
                let previous = previous.unwrap_or_default();
                self.put(ModelCatalogRow {
                    provider: claim.provider.into(),
                    catalog: if previous.catalog.is_null() { json!({ "version": 1, "models": [] }) } else { previous.catalog },
                    last_attempted_at: Some(claim.attempted_at),
                    ..previous
                })
            };
            Box::pin(async move { Ok(result) })
        }

        fn save_success<'a>(
            &'a self,
            provider: &'a str,
            catalog: &'a ModelCatalog,
            fetched_at: f64,
        ) -> BoxFuture<'a, Result<Option<ModelCatalogRow>, String>> {
            let previous = self.get(provider).unwrap_or_default();
            let row = self.put(ModelCatalogRow {
                provider: provider.into(),
                catalog: catalog.to_json(),
                fetched_at: Some(fetched_at),
                last_error: None,
                ..previous
            });
            Box::pin(async move { Ok(row) })
        }

        fn save_failure<'a>(&'a self, provider: &'a str, error: &'a str) -> BoxFuture<'a, Result<Option<ModelCatalogRow>, String>> {
            let previous = self.get(provider).unwrap_or_default();
            let row = self.put(ModelCatalogRow { provider: provider.into(), last_error: Some(error.into()), ..previous });
            Box::pin(async move { Ok(row) })
        }

        fn save_exclusions<'a>(
            &'a self,
            provider: &'a str,
            excluded_models: &'a [String],
            fallback_catalog: &'a ModelCatalog,
        ) -> BoxFuture<'a, Result<Option<ModelCatalogRow>, String>> {
            let previous = self.get(provider);
            let row = self.put(ModelCatalogRow {
                provider: provider.into(),
                catalog: previous.as_ref().map_or_else(|| fallback_catalog.to_json(), |row| row.catalog.clone()),
                excluded_models: excluded_models.iter().cloned().map(Some).collect(),
                ..previous.unwrap_or_default()
            });
            Box::pin(async move { Ok(row) })
        }
    }

    type Call = (String, Vec<String>, Option<String>);

    /// 실행 파일 이름을 제공자 이름 그대로 쓰는 호스트. 명령 출력은 실행 파일별로 바꿀 수 있다.
    #[derive(Default)]
    struct FakeHost {
        outputs: Mutex<HashMap<String, Result<String, String>>>,
        changed_at: Mutex<HashMap<String, f64>>,
        calls: Mutex<Vec<Call>>,
    }

    impl FakeHost {
        fn with_payloads() -> Arc<Self> {
            let host = Self::default();
            for (provider, payload) in
                [("codex", codex_payload()), ("antigravity", ANTIGRAVITY_PAYLOAD.into()), ("claude", claude_payload())]
            {
                host.outputs.lock().unwrap().insert(provider.into(), Ok(payload));
                host.changed_at.lock().unwrap().insert(provider.into(), 0.0);
            }
            Arc::new(host)
        }

        fn fetched(&self) -> Vec<String> {
            self.calls.lock().unwrap().iter().map(|call| call.0.clone()).collect()
        }
    }

    impl ModelCatalogHost for FakeHost {
        fn resolve_executable<'a>(&'a self, provider: &'a str) -> BoxFuture<'a, Result<String, String>> {
            Box::pin(async move { Ok(provider.to_string()) })
        }

        fn run_command<'a>(
            &'a self,
            executable: &'a str,
            arguments: &'a [&'a str],
            input: Option<&'a str>,
        ) -> BoxFuture<'a, Result<String, String>> {
            self.calls.lock().unwrap().push((
                executable.into(),
                arguments.iter().map(|argument| argument.to_string()).collect(),
                input.map(str::to_string),
            ));
            let output = self.outputs.lock().unwrap().get(executable).cloned().unwrap_or(Ok(String::new()));
            Box::pin(async move { output })
        }

        fn executable_changed_at<'a>(&'a self, executable: &'a str) -> BoxFuture<'a, Option<f64>> {
            let changed_at = self.changed_at.lock().unwrap().get(executable).copied();
            Box::pin(async move { changed_at })
        }
    }

    fn service_with(store: Arc<FakeStore>, host: Arc<FakeHost>, clock: Arc<Mutex<f64>>) -> Arc<ModelCatalogService> {
        let options = ModelCatalogOptions {
            now: Arc::new(move || *clock.lock().unwrap()),
            warn: Arc::new(|_, _| {}),
            ..ModelCatalogOptions::default()
        };
        ModelCatalogService::new(store, host, options)
    }

    #[tokio::test]
    async fn cached_antigravity_high_is_corrected_to_fixed_thinking_default() {
        let mut cached = parse_antigravity_model_catalog(ANTIGRAVITY_PAYLOAD).unwrap();
        let opus = cached.models.iter_mut().find(|entry| entry.id == "claude-opus-4-6-thinking").unwrap();
        opus.efforts = vec!["high".into()];
        opus.default_effort = "high".into();
        let store = Arc::new(FakeStore::default());
        store.put(ModelCatalogRow { provider: "antigravity".into(), catalog: cached.to_json(), ..Default::default() });
        let host = Arc::new(FakeHost::default());
        host.outputs.lock().unwrap().insert("antigravity".into(), Err("must not fetch".into()));
        let service = service_with(Arc::clone(&store), Arc::clone(&host), Arc::new(Mutex::new(now_milliseconds())));
        service.load_cached().await.unwrap();
        let capabilities = service.model_capabilities("antigravity", "claude-opus-4-6-thinking").unwrap().unwrap();
        assert_eq!(capabilities.efforts, ["default"]);
        assert_eq!(normalize_catalog(&store.get("antigravity").unwrap().catalog).models[2].default_effort, "high");
        assert!(host.fetched().is_empty());
    }

    #[test]
    fn missing_models_leave_the_list_but_stay_for_validation() {
        let newer = json!({ "models": [{
            "slug": "gpt-newer", "display_name": "GPT Newer", "visibility": "list", "default_reasoning_level": "high",
            "supported_reasoning_levels": [{ "effort": "high" }],
        }] });
        let merged = merge_model_catalog(
            &parse_codex_model_catalog(&codex_payload()).unwrap(),
            &parse_codex_model_catalog(&newer.to_string()).unwrap(),
            now_milliseconds(),
        )
        .unwrap();
        let state: Vec<(&str, bool)> = merged.models.iter().map(|entry| (entry.id.as_str(), entry.available)).collect();
        assert_eq!(state, [("gpt-newer", true), ("gpt-next", false)]);
    }

    #[tokio::test]
    async fn service_keeps_twelve_hour_gate_and_saves_exclusions() {
        let clock = Arc::new(Mutex::new(time("2026-09-05T00:00:00Z")));
        let store = Arc::new(FakeStore::default());
        let host = FakeHost::with_payloads();
        let service = service_with(Arc::clone(&store), Arc::clone(&host), Arc::clone(&clock));
        service.load_cached().await.unwrap();
        let first = service.refresh_due(false).await.unwrap();
        assert_eq!(statuses(&first), ["updated", "updated", "updated"]);
        assert_eq!(host.calls.lock().unwrap().len(), 3);
        // Claude는 프롬프트 없이 initialize 요청만 stdin으로 보내야 비용이 없다.
        let claude_calls: Vec<Call> = host.calls.lock().unwrap().iter().filter(|call| call.0 == "claude").cloned().collect();
        assert_eq!(claude_calls.len(), 1);
        assert!(claude_calls[0].1.iter().any(|argument| argument == "--input-format"));
        assert!(claude_calls[0].2.as_deref().unwrap().contains("\"subtype\":\"initialize\""));
        assert!(service.model_capabilities("claude", "opus[1m]").unwrap().unwrap().supports_fast_mode);
        // 내장 기본 목록의 기존 설정값은 선택 목록에서 내려가되 검증용으로 남는다.
        assert!(!service.model_capabilities("claude", "claude-opus-5").unwrap().unwrap().available);

        *clock.lock().unwrap() += MODEL_CATALOG_REFRESH_MILLISECONDS - 1.0;
        let fresh = service.refresh_due(false).await.unwrap();
        assert_eq!(statuses(&fresh), ["fresh", "fresh", "fresh"]);
        assert_eq!(host.calls.lock().unwrap().len(), 3);

        let rejected = service.set_exclusions("codex", Some(&json!(["gpt-next"]))).await;
        assert!(matches!(rejected, Err(ModelCatalogError::Validation(_))));

        // 단일 모델을 모두 제외할 수 없으므로 모델을 하나 더 발견시킨다.
        let mut current = store.get("codex").unwrap();
        let mut second = current.catalog["models"][0].clone();
        second["id"] = json!("gpt-second");
        second["title"] = json!("GPT Second");
        current.catalog["models"].as_array_mut().unwrap().push(second);
        store.put(current.clone());
        service.set_row("codex", normalized_row(current));
        let saved = service.set_exclusions("codex", Some(&json!(["gpt-next"]))).await.unwrap();
        let codex = saved["providers"].as_array().unwrap().iter().find(|entry| entry["backend"] == "codex").unwrap();
        assert_eq!(codex["excludedModels"], json!(["gpt-next"]));
    }

    #[tokio::test]
    async fn failed_refresh_keeps_last_good_models_and_exclusions() {
        let store = Arc::new(FakeStore::default());
        let host = FakeHost::with_payloads();
        let service = service_with(Arc::clone(&store), Arc::clone(&host), Arc::new(Mutex::new(now_milliseconds())));
        service.load_cached().await.unwrap();
        service.refresh_provider("codex", true).await.unwrap();
        service.set_exclusions("codex", Some(&json!([]))).await.unwrap();

        host.outputs.lock().unwrap().insert("codex".into(), Err("offline".into()));
        let failed = service.refresh_provider("codex", true).await.unwrap();
        assert_eq!(failed["status"], "failed");
        assert_eq!(service.model_capabilities("codex", "gpt-next").unwrap().unwrap().id, "gpt-next");
        assert_eq!(service.snapshot()["providers"][0]["lastError"], "offline");
    }

    #[tokio::test]
    async fn refetches_only_when_cli_was_installed_after_last_attempt() {
        let clock = Arc::new(Mutex::new(time("2026-09-23T10:53:00Z")));
        let host = FakeHost::with_payloads();
        let service = service_with(Arc::new(FakeStore::default()), Arc::clone(&host), Arc::clone(&clock));
        service.load_cached().await.unwrap();
        service.refresh_due(false).await.unwrap();
        host.calls.lock().unwrap().clear();

        *clock.lock().unwrap() += 60_000.0;
        let unchanged = service.refresh_updated_executables().await.unwrap();
        assert_eq!(statuses(&unchanged), ["fresh", "fresh", "fresh"]);
        assert!(host.fetched().is_empty());

        // 수집 4분 뒤 Claude CLI만 새로 설치됐다.
        host.changed_at.lock().unwrap().insert("claude".into(), time("2026-09-23T10:57:34Z"));
        *clock.lock().unwrap() = time("2026-09-23T11:03:00Z");
        let updated = service.refresh_updated_executables().await.unwrap();
        assert_eq!(statuses(&updated), ["fresh", "fresh", "updated"]);
        assert_eq!(host.fetched(), ["claude"]);

        // 한 번 다시 받은 뒤에는 같은 설치로 반복 수집하지 않는다.
        *clock.lock().unwrap() += 60_000.0;
        service.refresh_updated_executables().await.unwrap();
        assert_eq!(host.fetched(), ["claude"]);
    }

    #[test]
    fn migrations_prepare_model_catalog_and_dynamic_efforts() {
        let Ok(directory) = std::env::var("OFFICE_MIGRATIONS_DIR") else {
            eprintln!("OFFICE_MIGRATIONS_DIR이 없어 건너뜀");
            return;
        };
        let read = |name: &str| std::fs::read_to_string(std::path::Path::new(&directory).join(name)).unwrap();
        let source = read("036_agent_model_catalog.sql");
        assert!(source.contains("CREATE TABLE IF NOT EXISTS agent_model_catalogs"));
        assert!(source.contains("excluded_models text[]"));
        assert!(source.contains("effort ~ '^[a-z]"));
        let claude = read("037_claude_model_catalog.sql");
        assert!(claude.contains("DROP CONSTRAINT IF EXISTS agent_model_catalogs_provider_check"));
        assert!(claude.contains("CHECK (provider IN ('codex', 'antigravity', 'claude'))"));
    }

    #[tokio::test]
    async fn catalog_command_follows_exec_file_results() {
        let second = Duration::from_secs(5);
        let run = |program: &'static str, arguments: &'static [&'static str], input, timeout, max| {
            run_catalog_command(program, arguments, input, timeout, max)
        };
        assert_eq!(run("/bin/cat", &[], Some("안녕\n"), second, 1_024).await, Ok("안녕\n".into()));
        assert_eq!(
            run("/bin/sh", &["-c", "echo err >&2; exit 3"], None, second, 1_024).await,
            Err("Command failed: /bin/sh -c echo err >&2; exit 3\nerr\n".into())
        );
        assert_eq!(
            run("/nonexistent/officestra-cli", &[], None, second, 1_024).await,
            Err("spawn /nonexistent/officestra-cli ENOENT".into())
        );
        // ENOENT·EACCES 등이 아닌 실행 실패에는 Node처럼 경로를 붙이지 않는다.
        assert_eq!(run("/etc/hosts/x", &[], None, second, 1_024).await, Err("spawn ENOTDIR".into()));
        let long = format!("/tmp/{}", "a".repeat(300));
        assert_eq!(run_catalog_command(&long, &[], None, second, 1_024).await, Err("spawn ENAMETOOLONG".into()));
        assert_eq!(
            run("/bin/sh", &["-c", "head -c 5000 /dev/zero"], None, second, 100).await,
            Err("stdout maxBuffer length exceeded".into())
        );
        // 입력이 없으면 stdin을 열어 두므로 읽기를 기다리다 시간 초과로 끊긴다.
        assert_eq!(
            run("/bin/sh", &["-c", "read line; echo got"], None, Duration::from_millis(300), 1_024).await,
            Err("Command failed: /bin/sh -c read line; echo got\n".into())
        );
        // SIGTERM을 받고도 조용히 0으로 끝나면 Node처럼 성공이다.
        let short = Duration::from_millis(300);
        assert_eq!(
            run("/bin/sh", &["-c", "trap 'exit 0' TERM; echo hi; while true; do sleep 0.05; done"], None, short, 1_024).await,
            Ok("hi\n".into())
        );
        // 끊은 뒤에는 출력 파이프가 닫혀 있으므로 신호 처리 중 출력하면 SIGPIPE로 실패한다(Node 24 실측과 같음).
        for script in [
            "trap 'echo bye; exit 0' TERM; echo hi; while true; do sleep 0.05; done",
            "trap 'echo bye >&2; exit 0' TERM; echo hi; while true; do sleep 0.05; done",
        ] {
            assert_eq!(
                run_catalog_command("/bin/sh", &["-c", script], None, short, 1_024).await,
                Err(format!("Command failed: /bin/sh -c {script}\n")),
                "{script}"
            );
        }
    }
}
