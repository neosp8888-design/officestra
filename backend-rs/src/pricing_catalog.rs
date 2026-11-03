// 공식 가격표를 읽어 Codex와 Antigravity의 토큰 비용 계산에 제공한다. Node pricing-catalog.mjs를 옮겼다.
//
// Node는 모듈 전역 runtimeCatalogs 하나를 쓴다. 여기서는 같은 역할을 PricingCatalogs 값이 맡고,
// 운영은 PricingCatalogs::shared() 하나를 쓴다. 테스트는 각자 새 값을 만들어 서로 섞이지 않는다.

use std::collections::HashMap;
use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock, Mutex, PoisonError, RwLock};
use std::time::Duration;

use futures_util::stream::{FuturesUnordered, StreamExt};
use regex::Regex;
use serde_json::{Map, Value, json};
use sqlx::PgPool;

use crate::js::{
    date_parse, iso_string, js_max, js_string, js_to_number, js_trim, js_truthy, now_milliseconds, string_to_number,
    to_js_json, utc_day_milliseconds, utf16_prefix,
};

pub const PRICING_CATALOG_REFRESH_MILLISECONDS: f64 = 12.0 * 60.0 * 60.0 * 1_000.0;
/// 고정 열 위치로 읽던 카탈로그는 유효 기간이 남아 있어도 재사용하지 않는다.
pub const OPENAI_PRICING_PARSER_VERSION: u64 = 2;
const MAX_SOURCE_BYTES: usize = 6 * 1_024 * 1_024;
const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_millis(15_000);
const LONG_CONTEXT_THRESHOLD: f64 = 272_000.0;

/// 공급자별 공식 가격표 주소와 해석기. Node PRICING_CATALOG_SOURCES와 같은 순서다.
pub struct PricingSource {
    pub provider: &'static str,
    pub url: &'static str,
    pub parse: fn(&str) -> Result<PricingCatalog, String>,
}

pub static PRICING_CATALOG_SOURCES: [PricingSource; 2] = [
    PricingSource {
        provider: "codex",
        url: "https://developers.openai.com/api/docs/pricing.md",
        parse: parse_openai_pricing_markdown,
    },
    PricingSource {
        provider: "antigravity",
        url: "https://ai.google.dev/gemini-api/docs/pricing?hl=en",
        parse: parse_gemini_pricing_html,
    },
];

fn pricing_source(provider: &str) -> Option<&'static PricingSource> {
    PRICING_CATALOG_SOURCES.iter().find(|source| source.provider == provider)
}

/// 단가 규칙 하나. 없는 칸은 JSON에 쓰지 않는다(Node compactRate).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PricingRule {
    pub min_prompt_tokens: Option<f64>,
    pub max_prompt_tokens: Option<f64>,
    pub effective_from: Option<String>,
    pub effective_until: Option<String>,
    pub input: f64,
    pub cached: Option<f64>,
    pub cache_write: Option<f64>,
    pub output: f64,
}

impl PricingRule {
    fn rate(input: f64, cached: Option<f64>, cache_write: Option<f64>, output: f64) -> Self {
        Self { input, cached, cache_write, output, ..Self::default() }
    }

    /// 저장되는 정규화 규칙의 JSON. Node normalizedRule처럼 단가 넷 뒤에 토큰 경계와 기간이 온다.
    pub fn to_json(&self) -> Value {
        let mut fields = Map::new();
        fields.insert("input".into(), Value::from(self.input));
        if let Some(cached) = self.cached {
            fields.insert("cached".into(), Value::from(cached));
        }
        if let Some(cache_write) = self.cache_write {
            fields.insert("cacheWrite".into(), Value::from(cache_write));
        }
        fields.insert("output".into(), Value::from(self.output));
        if let Some(minimum) = self.min_prompt_tokens {
            fields.insert("minPromptTokens".into(), Value::from(minimum));
        }
        if let Some(maximum) = self.max_prompt_tokens {
            fields.insert("maxPromptTokens".into(), Value::from(maximum));
        }
        if let Some(from) = &self.effective_from {
            fields.insert("effectiveFrom".into(), Value::from(from.as_str()));
        }
        if let Some(until) = &self.effective_until {
            fields.insert("effectiveUntil".into(), Value::from(until.as_str()));
        }
        Value::Object(fields)
    }

    /// Node ruleMatches. 기간은 ISO 문자열을 그때그때 읽는다.
    fn matches(&self, prompt_tokens: f64, timestamp: f64) -> bool {
        if self.min_prompt_tokens.is_some_and(|minimum| prompt_tokens < minimum) {
            return false;
        }
        if self.max_prompt_tokens.is_some_and(|maximum| prompt_tokens > maximum) {
            return false;
        }
        let from = self.effective_from.as_deref().and_then(date_parse);
        let until = self.effective_until.as_deref().and_then(date_parse);
        !(from.is_some_and(|from| timestamp < from) || until.is_some_and(|until| timestamp >= until))
    }
}

/// pricingRateFor 결과. 입력·출력 단가는 늘 있고 캐시 단가는 없을 수 있다.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PricingRate {
    pub input: f64,
    pub cached: Option<f64>,
    pub cache_write: Option<f64>,
    pub output: f64,
}

impl PricingRate {
    pub fn to_json(&self) -> Value {
        let mut fields = Map::new();
        fields.insert("input".into(), Value::from(self.input));
        if let Some(cached) = self.cached {
            fields.insert("cached".into(), Value::from(cached));
        }
        if let Some(cache_write) = self.cache_write {
            fields.insert("cacheWrite".into(), Value::from(cache_write));
        }
        fields.insert("output".into(), Value::from(self.output));
        Value::Object(fields)
    }
}

/// 모델 하나의 standard·fast 규칙 묶음. 규칙이 하나도 없는 방식은 None이다.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PricingModes {
    pub standard: Option<Vec<PricingRule>>,
    pub fast: Option<Vec<PricingRule>>,
}

impl PricingModes {
    fn standard(rules: Vec<PricingRule>) -> Self {
        Self { standard: Some(rules), fast: None }
    }

    fn to_json(&self) -> Value {
        let rules = |rules: &[PricingRule]| Value::Array(rules.iter().map(PricingRule::to_json).collect());
        let mut fields = Map::new();
        if let Some(standard) = &self.standard {
            fields.insert("standard".into(), rules(standard));
        }
        if let Some(fast) = &self.fast {
            fields.insert("fast".into(), rules(fast));
        }
        Value::Object(fields)
    }
}

/// `{ version: 1, models }`. 모델은 넣은 순서를 지킨다.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PricingCatalog {
    pub models: Vec<(String, PricingModes)>,
}

impl PricingCatalog {
    pub fn get(&self, model: &str) -> Option<&PricingModes> {
        self.models.iter().find(|(name, _)| name == model).map(|(_, modes)| modes)
    }

    /// `models[model] = modes`. 이미 있으면 자리를 지키고 값만 바꾼다.
    fn set(&mut self, model: &str, modes: PricingModes) {
        match self.models.iter_mut().find(|(name, _)| name == model) {
            Some((_, current)) => *current = modes,
            None => self.models.push((model.to_string(), modes)),
        }
    }

    pub fn to_json(&self) -> Value {
        let models: Map<String, Value> =
            self.models.iter().map(|(model, modes)| (model.clone(), modes.to_json())).collect();
        json!({ "version": 1, "models": models })
    }
}

fn openai_model(standard: [f64; 8], fast: [f64; 8]) -> PricingModes {
    PricingModes { standard: Some(context_rules(standard)), fast: Some(context_rules(fast)) }
}

fn context_rules(values: [f64; 8]) -> Vec<PricingRule> {
    let [input, cached, cache_write, output, long_input, long_cached, long_cache_write, long_output] = values;
    vec![
        PricingRule {
            max_prompt_tokens: Some(LONG_CONTEXT_THRESHOLD),
            ..PricingRule::rate(input, Some(cached), Some(cache_write), output)
        },
        PricingRule {
            min_prompt_tokens: Some(LONG_CONTEXT_THRESHOLD + 1.0),
            ..PricingRule::rate(long_input, Some(long_cached), Some(long_cache_write), long_output)
        },
    ]
}

fn gemini_flash_rules() -> PricingModes {
    PricingModes::standard(vec![
        PricingRule {
            effective_until: Some("2027-01-01T00:00:00.000Z".into()),
            ..PricingRule::rate(0.75, Some(0.075), None, 3.75)
        },
        PricingRule {
            effective_from: Some("2027-01-01T00:00:00.000Z".into()),
            ..PricingRule::rate(1.5, Some(0.15), None, 7.5)
        },
    ])
}

fn gemini_pro_rules() -> PricingModes {
    PricingModes::standard(vec![
        PricingRule { max_prompt_tokens: Some(200_000.0), ..PricingRule::rate(2.0, Some(0.2), None, 12.0) },
        PricingRule { min_prompt_tokens: Some(200_001.0), ..PricingRule::rate(4.0, Some(0.4), None, 18.0) },
    ])
}

/// Node BUILTIN_PRICING_CATALOGS.
fn builtin_catalog(provider: &str) -> PricingCatalog {
    let models: Vec<(&str, PricingModes)> = match provider {
        "codex" => vec![
            ("gpt-6-astra", openai_model([10.0, 1.0, 12.5, 50.0, 20.0, 2.0, 25.0, 75.0], [20.0, 2.0, 25.0, 100.0, 40.0, 4.0, 50.0, 150.0])),
            ("gpt-5.6-sol", openai_model([4.0, 0.4, 5.0, 20.0, 8.0, 0.8, 10.0, 30.0], [8.0, 0.8, 10.0, 40.0, 16.0, 1.6, 20.0, 60.0])),
            ("gpt-5.6", openai_model([4.0, 0.4, 5.0, 20.0, 8.0, 0.8, 10.0, 30.0], [8.0, 0.8, 10.0, 40.0, 16.0, 1.6, 20.0, 60.0])),
            ("gpt-5.6-terra", openai_model([2.0, 0.2, 2.5, 12.0, 4.0, 0.4, 5.0, 18.0], [4.0, 0.4, 5.0, 24.0, 8.0, 0.8, 10.0, 36.0])),
            ("gpt-5.6-luna", openai_model([0.2, 0.02, 0.25, 1.2, 0.4, 0.04, 0.5, 1.8], [0.4, 0.04, 0.5, 2.4, 0.8, 0.08, 1.0, 3.6])),
        ],
        "antigravity" => vec![
            ("gemini-3.8-flash", gemini_flash_rules()),
            ("gemini-3.7-flash", gemini_flash_rules()),
            ("gemini-3.6-flash", gemini_flash_rules()),
            ("gemini-3.5-flash", PricingModes::standard(vec![PricingRule::rate(1.5, Some(0.15), None, 9.0)])),
            ("gemini-3.1-pro", gemini_pro_rules()),
            ("gemini-3.1-pro-preview", gemini_pro_rules()),
        ],
        _ => Vec::new(),
    };
    PricingCatalog { models: models.into_iter().map(|(model, modes)| (model.to_string(), modes)).collect() }
}

/// pricingRateFor 인자. prompt_tokens 기본값은 0, priced_at이 없거나 유한하지 않으면 지금 시각이다.
#[derive(Clone, Copy, Debug, Default)]
pub struct RateQuery<'a> {
    pub backend: Option<&'a str>,
    pub model: Option<&'a str>,
    pub fast_mode: bool,
    pub prompt_tokens: f64,
    pub priced_at: Option<f64>,
}

/// 실행 중 단가표(Node runtimeCatalogs). 내장 기본값에서 시작해 공식 가격표를 덧씌운다.
pub struct PricingCatalogs {
    catalogs: RwLock<HashMap<&'static str, PricingCatalog>>,
}

impl Default for PricingCatalogs {
    fn default() -> Self {
        Self::new()
    }
}

impl PricingCatalogs {
    pub fn new() -> Self {
        Self { catalogs: RwLock::new(Self::builtins()) }
    }

    fn builtins() -> HashMap<&'static str, PricingCatalog> {
        PRICING_CATALOG_SOURCES.iter().map(|source| (source.provider, builtin_catalog(source.provider))).collect()
    }

    /// 운영 백엔드가 함께 쓰는 단가표 하나.
    pub fn shared() -> Arc<Self> {
        static SHARED: LazyLock<Arc<PricingCatalogs>> = LazyLock::new(|| Arc::new(PricingCatalogs::new()));
        Arc::clone(&SHARED)
    }

    /// installPricingCatalog. 지금 단가표에 새 값을 덧씌우고 합친 결과를 돌려준다.
    pub fn install(&self, provider: &str, value: &Value) -> Result<PricingCatalog, String> {
        self.install_catalog(provider, &normalized_catalog(value))
    }

    fn install_catalog(&self, provider: &str, update: &PricingCatalog) -> Result<PricingCatalog, String> {
        let Some(source) = pricing_source(provider) else {
            return Err(format!("지원하지 않는 가격 제공자입니다: {provider}"));
        };
        let mut catalogs = self.catalogs.write().unwrap_or_else(PoisonError::into_inner);
        let merged = merge_catalogs(catalogs.get(source.provider).unwrap_or(&PricingCatalog::default()), update);
        catalogs.insert(source.provider, merged.clone());
        Ok(merged)
    }

    /// resetPricingCatalogsForTesting.
    pub fn reset(&self) {
        *self.catalogs.write().unwrap_or_else(PoisonError::into_inner) = Self::builtins();
    }

    /// currentPricingCatalog. 모르는 공급자는 빈 단가표다.
    pub fn current(&self, provider: &str) -> PricingCatalog {
        self.catalogs.read().unwrap_or_else(PoisonError::into_inner).get(provider).cloned().unwrap_or_default()
    }

    /// pricingRateFor. Codex만 fast 단가를 쓰고, 토큰 경계와 기간이 맞는 첫 규칙을 고른다.
    pub fn rate_for(&self, query: &RateQuery<'_>) -> Option<PricingRate> {
        let provider = match query.backend {
            Some("codex") => "codex",
            Some("antigravity") => "antigravity",
            _ => return None,
        };
        let catalogs = self.catalogs.read().unwrap_or_else(PoisonError::into_inner);
        let modes = catalogs.get(provider)?.get(query.model.unwrap_or(""))?;
        let rules = if provider == "codex" && query.fast_mode { modes.fast.as_ref() } else { modes.standard.as_ref() }?;
        let timestamp = query.priced_at.filter(|timestamp| timestamp.is_finite()).unwrap_or_else(now_milliseconds);
        let tokens = if query.prompt_tokens.is_finite() { js_max(0.0, query.prompt_tokens) } else { 0.0 };
        rules.iter().find(|rule| rule.matches(tokens, timestamp)).map(|rule| PricingRate {
            input: rule.input,
            cached: rule.cached,
            cache_write: rule.cache_write,
            output: rule.output,
        })
    }
}

/// mergePricingCatalog. 새 단가표에 없는 옛 모델의 마지막 정상 단가를 지킨다.
pub fn merge_pricing_catalog(base: &Value, update: &Value) -> PricingCatalog {
    merge_catalogs(&normalized_catalog(base), &normalized_catalog(update))
}

/// 정규화된 두 단가표를 합친다. 모델마다 새 쪽에 있는 방식만 바꾼다(`{...old, ...new}`).
fn merge_catalogs(left: &PricingCatalog, right: &PricingCatalog) -> PricingCatalog {
    let mut merged = left.clone();
    for (model, modes) in &right.models {
        let mut combined = merged.get(model).cloned().unwrap_or_default();
        if modes.standard.is_some() {
            combined.standard.clone_from(&modes.standard);
        }
        if modes.fast.is_some() {
            combined.fast.clone_from(&modes.fast);
        }
        merged.set(model, combined);
    }
    merged
}

/// Node normalizedCatalog. 저장값이나 외부 JSON에서 올바른 모델과 규칙만 남긴다.
pub fn normalized_catalog(value: &Value) -> PricingCatalog {
    let mut catalog = PricingCatalog::default();
    let Some(models) = value.as_object().and_then(|fields| fields.get("models")).and_then(Value::as_object) else {
        return catalog;
    };
    for (model, modes) in models {
        // 배열은 typeof "object"라 통과하지만 standard·fast 키가 없어 결국 빠진다.
        let Some(modes) = modes.as_object().filter(|_| valid_model_id(model)) else {
            continue;
        };
        let rules = |mode: &str| {
            let rules: Vec<PricingRule> =
                modes.get(mode).and_then(Value::as_array)?.iter().filter_map(normalized_rule).collect();
            (!rules.is_empty()).then_some(rules)
        };
        let normalized = PricingModes { standard: rules("standard"), fast: rules("fast") };
        if normalized.standard.is_some() || normalized.fast.is_some() {
            catalog.set(model, normalized);
        }
    }
    catalog
}

/// Node normalizedRule. 값은 JS Number()·Date.parse 규칙으로 읽는다(null 토큰 경계는 0이 된다).
fn normalized_rule(value: &Value) -> Option<PricingRule> {
    let fields = value.as_object()?;
    let input = safe_price(fields.get("input"))?;
    let output = safe_price(fields.get("output"))?;
    // 0 이상의 수에서 Math.round와 f64::round는 같다.
    let tokens = |field: &str| {
        fields.get(field).map(js_to_number).filter(|number| number.is_finite() && *number >= 0.0).map(f64::round)
    };
    let date = |field: &str| fields.get(field).and_then(|value| date_parse(&js_string(value))).map(iso_string);
    Some(PricingRule {
        min_prompt_tokens: tokens("minPromptTokens"),
        max_prompt_tokens: tokens("maxPromptTokens"),
        effective_from: date("effectiveFrom"),
        effective_until: date("effectiveUntil"),
        input,
        cached: safe_price(fields.get("cached")),
        cache_write: safe_price(fields.get("cacheWrite")),
        output,
    })
}

/// Node safePrice. undefined·null·빈 문자열은 없음, 나머지는 Number()가 0~10000이어야 한다.
fn safe_price(value: Option<&Value>) -> Option<f64> {
    match value? {
        Value::Null => None,
        Value::String(text) => safe_price_text(text),
        value => finite_price(js_to_number(value)),
    }
}

fn safe_price_text(text: &str) -> Option<f64> {
    if text.is_empty() { None } else { finite_price(string_to_number(text)) }
}

fn finite_price(number: f64) -> Option<f64> {
    (number.is_finite() && (0.0..=10_000.0).contains(&number)).then_some(number)
}

/// `/^[a-z0-9][a-z0-9._:-]{1,119}$/`.
fn valid_model_id(value: &str) -> bool {
    let bytes = value.as_bytes();
    (2..=120).contains(&bytes.len())
        && matches!(bytes[0], b'a'..=b'z' | b'0'..=b'9')
        && bytes[1..].iter().all(|byte| matches!(byte, b'a'..=b'z' | b'0'..=b'9' | b'.' | b'_' | b':' | b'-'))
}

/// 정규식의 JS `\s`(WhiteSpace와 LineTerminator). Rust `\s`는 U+0085를 넣고 U+FEFF를 빼서 쓰지 않는다.
macro_rules! js_space {
    () => {
        r"[\t\n\x0B\x0C\r \x{A0}\x{1680}\x{2000}-\x{200A}\x{2028}\x{2029}\x{202F}\x{205F}\x{3000}\x{FEFF}]"
    };
}

fn pattern(source: &str) -> Regex {
    Regex::new(source).expect("가격표 정규식")
}

// JS 정규식의 i 깃발(유니코드 없이)은 ASCII 글자만 대소문자를 겹치므로 (?i-u:…)로 낱말에만 건다.
// \b도 ASCII 낱말 경계라 (?-u:\b)다.
static OPENAI_NOTE: LazyLock<Regex> = LazyLock::new(|| pattern(concat!(js_space!(), r"*\(<[^)]*\)", js_space!(), r"*$")));
static GEMINI_HEADING: LazyLock<Regex> =
    LazyLock::new(|| pattern(r#"<(?i-u:h2)(?-u:\b)[^>]*(?i-u:id)="([^"]+)"[^>]*>"#));
static GEMINI_STANDARD: LazyLock<Regex> = LazyLock::new(|| {
    pattern(concat!(
        r#"<(?i-u:h3)(?-u:\b)[^>]*(?:(?i-u:data-text="Standard")|>"#,
        js_space!(),
        r"*(?i-u:Standard)",
        js_space!(),
        r"*<)"
    ))
});
static GEMINI_TABLE: LazyLock<Regex> =
    LazyLock::new(|| pattern(r"<(?i-u:table)(?-u:\b)[^>]*>((?s:.)*?)</(?i-u:table)>"));
static GEMINI_CODE: LazyLock<Regex> =
    LazyLock::new(|| pattern(r"<(?i-u:code)(?-u:\b)[^>]*>((?i-u:gemini-)[^<]+)</(?i-u:code)>"));
static GEMINI_ROW: LazyLock<Regex> = LazyLock::new(|| pattern(r"<(?i-u:tr)(?-u:\b)[^>]*>((?s:.)*?)</(?i-u:tr)>"));
static GEMINI_CELL: LazyLock<Regex> = LazyLock::new(|| pattern(r"<(?i-u:td)(?-u:\b)[^>]*>((?s:.)*?)</(?i-u:td)>"));
static GEMINI_BREAK: LazyLock<Regex> =
    LazyLock::new(|| pattern(concat!(r"<(?i-u:br)", js_space!(), r"*/?", js_space!(), r"*>")));
static STORAGE_PRICE: LazyLock<Regex> = LazyLock::new(|| pattern(r"(?i-u:storage price)"));
static DOLLAR_PRICE: LazyLock<Regex> =
    LazyLock::new(|| pattern(concat!(r"\$", js_space!(), r"*([0-9]+(?:[.,][0-9]+)?)")));
static AT_MOST: LazyLock<Regex> = LazyLock::new(|| {
    pattern(concat!(r"(?:<=|≤)", js_space!(), r"*([0-9.]+)", js_space!(), r"*((?i-u:[km])?)"))
});
static MORE_THAN: LazyLock<Regex> =
    LazyLock::new(|| pattern(concat!(r">", js_space!(), r"*([0-9.]+)", js_space!(), r"*((?i-u:[km])?)")));
static THROUGH_DATE: LazyLock<Regex> = LazyLock::new(|| {
    pattern(concat!(
        r"(?i-u:through)",
        js_space!(),
        r"+([A-Za-z]+",
        js_space!(),
        r"+[0-9]{1,2},",
        js_space!(),
        r"+[0-9]{4})"
    ))
});
static STARTING_DATE: LazyLock<Regex> = LazyLock::new(|| {
    pattern(concat!(
        r"(?i-u:starting)",
        js_space!(),
        r"+([A-Za-z]+",
        js_space!(),
        r"+[0-9]{1,2},",
        js_space!(),
        r"+[0-9]{4})"
    ))
});
static HTML_TAG: LazyLock<Regex> = LazyLock::new(|| pattern(r"<[^>]*>"));
static SPACE_RUN: LazyLock<Regex> = LazyLock::new(|| pattern(concat!(js_space!(), "+")));

/// Node parseOpenAIPricingMarkdown. Standard·Fast 표에서 짧은 문맥과 27만 2천 토큰 넘는 긴 문맥 단가를 읽는다.
pub fn parse_openai_pricing_markdown(source: &str) -> Result<PricingCatalog, String> {
    let mut catalog = PricingCatalog::default();
    for (heading, fast) in [("### Standard pricing data", false), ("### Fast pricing data", true)] {
      for section in source.match_indices(heading).map(|(offset, _)| markdown_section(&source[offset..], heading)) {
        let mut header = Vec::new();
        for line in section.split('\n') {
            if !js_trim(line).starts_with('|') {
                continue;
            }
            let cells = markdown_cells(line);
            if cells.first().is_some_and(|cell| cell == "Model") {
                header = cells;
                continue;
            }
            let divider = cells.first().is_some_and(|cell| !cell.is_empty() && cell.chars().all(|c| c == '-'));
            if cells.len() != header.len() || header.is_empty() || divider {
                continue;
            }
            let model = normalize_openai_model_id(&cells[0]);
            if !valid_model_id(&model) {
                continue;
            }
            let Some(short) = openai_rate_from_columns(&header, &cells, "Short context") else {
                continue;
            };
            let rules = match openai_rate_from_columns(&header, &cells, "Long context") {
                Some(long) => vec![
                    PricingRule { max_prompt_tokens: Some(LONG_CONTEXT_THRESHOLD), ..short },
                    PricingRule { min_prompt_tokens: Some(LONG_CONTEXT_THRESHOLD + 1.0), ..long },
                ],
                None => vec![short],
            };
            let mut modes = catalog.get(&model).cloned().unwrap_or_default();
            if fast {
                modes.fast = Some(rules);
            } else {
                modes.standard = Some(rules);
            }
            catalog.set(&model, modes);
        }
      }
    }
    if catalog.models.is_empty() {
        return Err("OpenAI 가격표에서 모델 단가를 찾지 못했습니다.".into());
    }
    if let Some(sol) = catalog.get("gpt-5.6-sol").cloned()
        && catalog.get("gpt-5.6").is_none()
    {
        catalog.set("gpt-5.6", sol);
    }
    Ok(catalog)
}

/// 제목 뒤부터 다음 `###` 제목 줄 앞까지. JS `/^###\s+/m`의 ^는 \n·\r·U+2028·U+2029 뒤에서 맞는다.
fn markdown_section<'a>(text: &'a str, heading: &str) -> &'a str {
    let Some(start) = text.find(heading) else {
        return "";
    };
    let rest = &text[start + heading.len()..];
    let line_starts = std::iter::once(0).chain(
        rest.char_indices()
            .filter(|(_, c)| matches!(c, '\n' | '\r' | '\u{2028}' | '\u{2029}'))
            .map(|(index, c)| index + c.len_utf8()),
    );
    let next = line_starts.into_iter().find(|&index| {
        rest[index..]
            .strip_prefix("###")
            .and_then(|after| after.chars().next())
            .is_some_and(crate::js::is_js_whitespace)
    });
    next.map_or(rest, |index| &rest[..index])
}

fn markdown_cells(line: &str) -> Vec<String> {
    let line = js_trim(line);
    let line = line.strip_prefix('|').unwrap_or(line);
    let line = line.strip_suffix('|').unwrap_or(line);
    line.split('|').map(|cell| js_trim(cell).to_string()).collect()
}

fn normalize_openai_model_id(value: &str) -> String {
    let stripped: String = value.chars().filter(|c| !matches!(c, '`' | '*' | '_')).collect();
    js_trim(&OPENAI_NOTE.replacen(&stripped, 1, "")).to_lowercase()
}

fn openai_rate_from_columns(header: &[String], cells: &[String], context: &str) -> Option<PricingRule> {
    let column = |suffix: &str| header.iter().position(|name| name == &format!("{context} {suffix}"));
    let price = |index: usize| cells.get(index).and_then(|cell| markdown_price(cell));
    // 새 표는 output이 앞에 오고 30m/12h 쓰기를 분리한다. 30m가 기존 기본 쓰기 단가다.
    // 필수 헤더를 모르면 추측해서 가격을 저장하지 않는다.
    let input = price(column("input")?)?;
    let output = price(column("output")?)?;
    let cached = price(column("cached input")?);
    let cache_write = price(column("30m write").or_else(|| column("cache writes"))?);
    Some(PricingRule::rate(input, cached, cache_write, output))
}

/// `/^\$?([0-9]+(?:\.[0-9]+)?)/`로 앞쪽 금액만 읽는다.
fn markdown_price(value: &str) -> Option<f64> {
    let cleaned: String = value.chars().filter(|c| !matches!(c, ',' | '`' | '*')).collect();
    let text = js_trim(&cleaned);
    if text.is_empty() || text == "-" {
        return None;
    }
    let digits = text.strip_prefix('$').unwrap_or(text);
    let integer = digits.bytes().take_while(u8::is_ascii_digit).count();
    if integer == 0 {
        return None;
    }
    let mut end = integer;
    if digits.as_bytes().get(end) == Some(&b'.') {
        let fraction = digits[end + 1..].bytes().take_while(u8::is_ascii_digit).count();
        if fraction > 0 {
            end += 1 + fraction;
        }
    }
    safe_price_text(&digits[..end])
}

/// Node parseGeminiPricingHTML. `gemini-` 절마다 Standard 표의 유료 칸을 읽고 preview 별칭을 단다.
pub fn parse_gemini_pricing_html(source: &str) -> Result<PricingCatalog, String> {
    let mut catalog = PricingCatalog::default();
    let headings: Vec<(usize, String)> = GEMINI_HEADING
        .captures_iter(source)
        .filter_map(|captures| Some((captures.get(0)?.start(), captures.get(1)?.as_str().to_string())))
        .collect();
    for (index, (start, identifier)) in headings.iter().enumerate() {
        let section_id = identifier.to_lowercase();
        if !section_id.starts_with("gemini-") {
            continue;
        }
        let end = headings.get(index + 1).map_or(source.len(), |(next, _)| *next);
        let section = &source[*start..end];
        let Some(standard) = GEMINI_STANDARD.find(section) else {
            continue;
        };
        let header = &section[..standard.start()];
        let table = GEMINI_TABLE
            .captures(&section[standard.start()..])
            .and_then(|captures| captures.get(1))
            .map(|table| table.as_str())
            .unwrap_or("");
        if table.is_empty() {
            continue;
        }
        let rules = combine_gemini_price_rules(&gemini_standard_price_cells(table));
        if rules.is_empty() {
            continue;
        }
        let mut identifiers: Vec<String> = Vec::new();
        for captures in GEMINI_CODE.captures_iter(header) {
            let identifier = html_text(&captures[1]).to_lowercase();
            if valid_model_id(&identifier) && !identifiers.contains(&identifier) {
                identifiers.push(identifier);
            }
        }
        if identifiers.is_empty() && valid_model_id(&section_id) {
            identifiers.push(section_id);
        }
        for identifier in &identifiers {
            catalog.set(identifier, PricingModes::standard(rules.clone()));
            if let Some(base) = identifier.strip_suffix("-preview")
                && catalog.get(base).is_none()
            {
                catalog.set(base, PricingModes::standard(rules.clone()));
            }
        }
    }
    if catalog.models.is_empty() {
        return Err("Gemini 가격표에서 모델 단가를 찾지 못했습니다.".into());
    }
    Ok(catalog)
}

/// Standard 표에서 입력·출력·컨텍스트 캐시 줄의 마지막(유료) 칸 HTML. 같은 줄이 여럿이면 뒤의 것이다.
#[derive(Default)]
struct GeminiPriceCells {
    input: Option<String>,
    output: Option<String>,
    cached: Option<String>,
}

fn gemini_standard_price_cells(table: &str) -> GeminiPriceCells {
    let mut result = GeminiPriceCells::default();
    for row in GEMINI_ROW.captures_iter(table) {
        let cells: Vec<&str> =
            GEMINI_CELL.captures_iter(&row[1]).filter_map(|cell| cell.get(1)).map(|cell| cell.as_str()).collect();
        if cells.len() < 2 {
            continue;
        }
        let label = html_text(cells[0]).to_lowercase();
        let paid = cells[cells.len() - 1].to_string();
        if label == "input price" {
            result.input = Some(paid.clone());
        }
        if label.starts_with("output price") {
            result.output = Some(paid.clone());
        }
        if label == "context caching price" {
            result.cached = Some(paid);
        }
    }
    result
}

/// 한 칸 안 `<br>` 조각마다의 금액과 조건(토큰 경계·기간).
struct DimensionRule {
    price: f64,
    min_prompt_tokens: Option<f64>,
    max_prompt_tokens: Option<f64>,
    effective_from: Option<String>,
    effective_until: Option<String>,
}

type RuleConstraints = (Option<f64>, Option<f64>, Option<String>, Option<String>);

/// 같은 조건끼리 입력·출력·캐시 금액을 모은다. 조건마다 처음 나온 금액을 쓴다(`??=`).
fn combine_gemini_price_rules(cells: &GeminiPriceCells) -> Vec<PricingRule> {
    let mut combined: Vec<(RuleConstraints, [Option<f64>; 3])> = Vec::new();
    for (field, cell) in [&cells.input, &cells.output, &cells.cached].into_iter().enumerate() {
        for rule in gemini_dimension_rules(cell.as_deref()) {
            let key = (rule.min_prompt_tokens, rule.max_prompt_tokens, rule.effective_from, rule.effective_until);
            let slot = match combined.iter().position(|(existing, _)| *existing == key) {
                Some(slot) => slot,
                None => {
                    combined.push((key, [None; 3]));
                    combined.len() - 1
                }
            };
            combined[slot].1[field].get_or_insert(rule.price);
        }
    }
    combined
        .into_iter()
        .filter_map(|((minimum, maximum, from, until), [input, output, cached])| {
            Some(PricingRule {
                min_prompt_tokens: minimum,
                max_prompt_tokens: maximum,
                effective_from: from,
                effective_until: until,
                ..PricingRule::rate(input?, cached, None, output?)
            })
        })
        .collect()
}

fn gemini_dimension_rules(value: Option<&str>) -> Vec<DimensionRule> {
    let Some(value) = value.filter(|value| !value.is_empty()) else {
        return Vec::new();
    };
    let mut rules = Vec::new();
    for fragment in GEMINI_BREAK.split(value) {
        let text = html_text(fragment);
        if text.is_empty() || STORAGE_PRICE.is_match(&text) {
            continue;
        }
        let Some(price) = DOLLAR_PRICE.captures(&text) else {
            continue;
        };
        let Some(price) = safe_price_text(&price[1].replacen(',', ".", 1)) else {
            continue;
        };
        let mut rule = DimensionRule {
            price,
            min_prompt_tokens: None,
            max_prompt_tokens: None,
            effective_from: None,
            effective_until: None,
        };
        if let Some(at_most) = AT_MOST.captures(&text) {
            rule.max_prompt_tokens = scaled_token_count(&at_most[1], &at_most[2]);
        }
        if let Some(more_than) = MORE_THAN.captures(&text)
            && let Some(boundary) = scaled_token_count(&more_than[1], &more_than[2])
        {
            rule.min_prompt_tokens = Some(boundary + 1.0);
        }
        if let Some(through) = THROUGH_DATE.captures(&text) {
            rule.effective_until = month_day_year_utc(&through[1]).map(|day| iso_string(day + 86_400_000.0));
        }
        if let Some(starting) = STARTING_DATE.captures(&text) {
            rule.effective_from = month_day_year_utc(&starting[1]).map(iso_string);
        }
        rules.push(rule);
    }
    rules
}

/// Node htmlText. 태그를 공백으로 바꾸고 몇 가지 문자 참조를 정해진 순서로 풀어 공백을 한 칸으로 줄인다.
fn html_text(value: &str) -> String {
    let text = HTML_TAG
        .replace_all(value, " ")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
        .replace("&nbsp;", " ")
        .replace("&#160;", " ")
        .replace("&#39;", "'")
        .replace("&apos;", "'")
        .replace("&quot;", "\"");
    js_trim(&SPACE_RUN.replace_all(&text, " ")).to_string()
}

fn scaled_token_count(value: &str, suffix: &str) -> Option<f64> {
    let number = string_to_number(value);
    if !number.is_finite() || number < 0.0 {
        return None;
    }
    let multiplier = match suffix.to_ascii_lowercase().as_str() {
        "m" => 1_000_000.0,
        "k" => 1_000.0,
        _ => 1.0,
    };
    Some((number * multiplier).round())
}

/// V8 Date.parse가 `"${value} 00:00:00 UTC"`를 읽는 옛 규칙. value는 정규식이 고른 "낱말 1~2자리, 4자리"이고
/// htmlText가 공백을 한 칸으로 줄였다. 앞 세 글자가 달 이름이면 그 달, 아니면 낱말을 버리고 숫자만으로
/// 연·월·일을 맞춘다. 0~49년은 2000년대, 50~99년은 1900년대로 읽는다.
fn month_day_year_utc(value: &str) -> Option<f64> {
    const MONTHS: [&str; 12] = ["jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec"];
    let (word, rest) = value.split_once(' ')?;
    let (first, second) = rest.split_once(", ")?;
    let (first, second): (i64, i64) = (first.parse().ok()?, second.parse().ok()?);
    let named = word
        .get(..3)
        .map(str::to_ascii_lowercase)
        .and_then(|prefix| MONTHS.iter().position(|month| *month == prefix))
        .map(|index| index as i64 + 1);
    let is_day = |value: i64| (1..=31).contains(&value);
    let (year, month, day) = match named {
        Some(month) if is_day(first) => (second, month, first),
        Some(month) => (first, month, second),
        None if is_day(first) => (1, first, second),
        None => (first, second, 1),
    };
    let year = match year {
        0..=49 => year + 2_000,
        50..=99 => year + 1_900,
        _ => year,
    };
    if !(1..=12).contains(&month) || !is_day(day) {
        return None;
    }
    Some(utc_day_milliseconds(year, month, day))
}

/// pricing_catalogs 한 줄. 시각은 밀리초이고 node-pg Date처럼 밀리초 아래는 버린다.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PricingCatalogRow {
    pub provider: String,
    pub source_url: String,
    pub catalog: Option<Value>,
    pub fetched_at: Option<f64>,
    pub last_attempted_at: Option<f64>,
    pub last_error: Option<String>,
}

/// claimRefresh 인자.
#[derive(Clone, Copy, Debug)]
pub struct RefreshClaim<'a> {
    pub provider: &'a str,
    pub source_url: &'a str,
    pub attempted_at: f64,
    pub minimum_interval_milliseconds: f64,
    pub force: bool,
}

/// 가격표 저장소. 운영은 PostgresPricingCatalogStore이고 테스트는 메모리 저장소를 쓴다.
pub trait PricingCatalogStore: Send + Sync + 'static {
    fn load(&self) -> impl Future<Output = Result<Vec<PricingCatalogRow>, String>> + Send;
    /// 마지막 시도에서 간격이 지났거나 force일 때만 시도 시각을 적고 그 줄을 돌려준다.
    fn claim_refresh(
        &self,
        claim: RefreshClaim<'_>,
    ) -> impl Future<Output = Result<Option<PricingCatalogRow>, String>> + Send;
    fn save_success(
        &self,
        provider: &str,
        catalog: &Value,
        fetched_at: f64,
    ) -> impl Future<Output = Result<Option<PricingCatalogRow>, String>> + Send;
    fn save_failure(
        &self,
        provider: &str,
        error: &str,
    ) -> impl Future<Output = Result<Option<PricingCatalogRow>, String>> + Send;
}

/// 받은 가격표 응답. 본문은 2xx일 때만 읽는다(Node는 ok가 아니면 text()를 부르지 않는다).
pub struct FetchedPage {
    pub status: u16,
    pub body: String,
}

pub trait PricingFetcher: Send + Sync + 'static {
    /// timeout은 연결부터 본문을 다 읽을 때까지다(Node AbortController와 같은 범위).
    fn fetch(&self, url: &str, timeout: Duration) -> impl Future<Output = Result<FetchedPage, String>> + Send;
}

/// Node fetch와 같은 요청 머리글로 공식 가격표를 받는다. Node fetch처럼 환경 프록시는 쓰지 않고 넘겨주기는 20번까지다.
pub struct HttpPricingFetcher {
    client: reqwest::Client,
}

impl HttpPricingFetcher {
    pub fn new() -> Result<Self, String> {
        let client = crate::http_client_builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::limited(20))
            .build()
            .map_err(|error| error.to_string())?;
        Ok(Self { client })
    }
}

impl PricingFetcher for HttpPricingFetcher {
    async fn fetch(&self, url: &str, timeout: Duration) -> Result<FetchedPage, String> {
        let response = self
            .client
            .get(url)
            .header("accept", "text/html,text/markdown;q=0.9,text/plain;q=0.8")
            .header("user-agent", "OFFICESTRA pricing catalog")
            .timeout(timeout)
            .send()
            .await
            .map_err(fetch_failure)?;
        let status = response.status().as_u16();
        if !(200..=299).contains(&status) {
            return Ok(FetchedPage { status, body: String::new() });
        }
        let bytes = response.bytes().await.map_err(fetch_failure)?;
        // Response.text()는 UTF-8 BOM을 떼고 잘못된 바이트를 U+FFFD로 바꾼다.
        let bytes = bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(&bytes);
        Ok(FetchedPage { status, body: String::from_utf8_lossy(bytes).into_owned() })
    }
}

/// Node 쪽 오류 문구에 맞춘다. 시간 초과는 AbortController 중단 문구, 그 밖의 연결 실패는 "fetch failed"다.
fn fetch_failure(error: reqwest::Error) -> String {
    if error.is_timeout() { "This operation was aborted".into() } else { "fetch failed".into() }
}

/// Node createPostgresPricingCatalogStore. 시각 인자는 ISO 문자열을 timestamptz로 바꿔 넣는다.
pub struct PostgresPricingCatalogStore {
    pool: PgPool,
}

impl PostgresPricingCatalogStore {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

macro_rules! pricing_row_columns {
    () => {
        "provider, source_url, catalog, \
         floor(extract(epoch FROM fetched_at) * 1000)::float8, \
         floor(extract(epoch FROM last_attempted_at) * 1000)::float8, \
         last_error"
    };
}

type PricingRowTuple = (String, String, Option<Value>, Option<f64>, Option<f64>, Option<String>);

fn pricing_row(
    (provider, source_url, catalog, fetched_at, last_attempted_at, last_error): PricingRowTuple,
) -> PricingCatalogRow {
    PricingCatalogRow { provider, source_url, catalog, fetched_at, last_attempted_at, last_error }
}

fn store_error(error: sqlx::Error) -> String {
    crate::rag::database_message(&error)
}

impl PricingCatalogStore for PostgresPricingCatalogStore {
    async fn load(&self) -> Result<Vec<PricingCatalogRow>, String> {
        let rows: Vec<PricingRowTuple> = sqlx::query_as(concat!("SELECT ", pricing_row_columns!(), " FROM pricing_catalogs"))
            .fetch_all(&self.pool)
            .await
            .map_err(store_error)?;
        Ok(rows.into_iter().map(pricing_row).collect())
    }

    async fn claim_refresh(&self, claim: RefreshClaim<'_>) -> Result<Option<PricingCatalogRow>, String> {
        let row: Option<PricingRowTuple> = sqlx::query_as(concat!(
            "INSERT INTO pricing_catalogs (provider, source_url, last_attempted_at, updated_at) ",
            "VALUES ($1, $2, $3::timestamptz, now()) ",
            "ON CONFLICT (provider) DO UPDATE SET ",
            "source_url = EXCLUDED.source_url, last_attempted_at = EXCLUDED.last_attempted_at, updated_at = now() ",
            "WHERE $5::boolean OR pricing_catalogs.last_attempted_at IS NULL ",
            "OR pricing_catalogs.last_attempted_at <= $3::timestamptz - ($4::double precision * interval '1 millisecond') ",
            "RETURNING ",
            pricing_row_columns!()
        ))
        .bind(claim.provider)
        .bind(claim.source_url)
        .bind(iso_string(claim.attempted_at))
        .bind(claim.minimum_interval_milliseconds)
        .bind(claim.force)
        .fetch_optional(&self.pool)
        .await
        .map_err(store_error)?;
        Ok(row.map(pricing_row))
    }

    async fn save_success(
        &self,
        provider: &str,
        catalog: &Value,
        fetched_at: f64,
    ) -> Result<Option<PricingCatalogRow>, String> {
        let text = String::from_utf8(to_js_json(catalog).map_err(|error| error.to_string())?)
            .map_err(|error| error.to_string())?;
        let row: Option<PricingRowTuple> = sqlx::query_as(concat!(
            "UPDATE pricing_catalogs SET catalog = $2::jsonb, fetched_at = $3::timestamptz, ",
            "last_error = NULL, updated_at = now() WHERE provider = $1 RETURNING ",
            pricing_row_columns!()
        ))
        .bind(provider)
        .bind(text)
        .bind(iso_string(fetched_at))
        .fetch_optional(&self.pool)
        .await
        .map_err(store_error)?;
        Ok(row.map(pricing_row))
    }

    async fn save_failure(&self, provider: &str, error: &str) -> Result<Option<PricingCatalogRow>, String> {
        let row: Option<PricingRowTuple> = sqlx::query_as(concat!(
            "UPDATE pricing_catalogs SET last_error = $2, updated_at = now() WHERE provider = $1 RETURNING ",
            pricing_row_columns!()
        ))
        .bind(provider)
        .bind(utf16_prefix(error, 1_000))
        .fetch_optional(&self.pool)
        .await
        .map_err(store_error)?;
        Ok(row.map(pricing_row))
    }
}

/// 서비스 설정. 기본값은 공용 단가표, Date.now, 표준 오류 출력 경고, 12시간 주기, 15초 제한이다.
#[derive(Clone)]
pub struct PricingServiceOptions {
    pub catalogs: Arc<PricingCatalogs>,
    pub now: Arc<dyn Fn() -> f64 + Send + Sync>,
    pub warn: Arc<dyn Fn(&str, &str) + Send + Sync>,
    pub refresh_interval_milliseconds: f64,
    pub request_timeout: Duration,
}

impl Default for PricingServiceOptions {
    fn default() -> Self {
        Self {
            catalogs: PricingCatalogs::shared(),
            now: Arc::new(now_milliseconds),
            warn: Arc::new(|message, detail| eprintln!("{message} {detail}")),
            refresh_interval_milliseconds: PRICING_CATALOG_REFRESH_MILLISECONDS,
            request_timeout: DEFAULT_REQUEST_TIMEOUT,
        }
    }
}

/// Node PricingCatalogService. 공급자별로 주기에 한 번만 공식 가격표를 받고, 실패하면 마지막 정상값을 지킨다.
pub struct PricingCatalogService<S, F> {
    store: S,
    fetcher: F,
    options: PricingServiceOptions,
    rows: Mutex<HashMap<String, PricingCatalogRow>>,
    timer: Mutex<Option<tokio::task::JoinHandle<()>>>,
    running: AtomicBool,
    stopped: AtomicBool,
}

impl<S: PricingCatalogStore, F: PricingFetcher> PricingCatalogService<S, F> {
    pub fn new(store: S, fetcher: F, options: PricingServiceOptions) -> Arc<Self> {
        Arc::new(Self {
            store,
            fetcher,
            options,
            rows: Mutex::new(HashMap::new()),
            timer: Mutex::new(None),
            running: AtomicBool::new(false),
            stopped: AtomicBool::new(true),
        })
    }

    pub fn store(&self) -> &S {
        &self.store
    }

    pub fn fetcher(&self) -> &F {
        &self.fetcher
    }

    fn row(&self, provider: &str) -> Option<PricingCatalogRow> {
        self.rows.lock().unwrap_or_else(PoisonError::into_inner).get(provider).cloned()
    }

    fn set_row(&self, row: PricingCatalogRow) {
        self.rows.lock().unwrap_or_else(PoisonError::into_inner).insert(row.provider.clone(), row);
    }

    /// 저장된 가격표를 단가표에 올린다. 모르는 공급자 줄은 건너뛴다.
    pub async fn load_cached(&self) -> Result<Vec<Value>, String> {
        for row in self.store.load().await? {
            if pricing_source(&row.provider).is_none() {
                continue;
            }
            let (provider, catalog) = (row.provider.clone(), row.catalog.clone());
            self.set_row(row);
            if provider == "codex" && catalog.as_ref().and_then(|value| value.get("openaiParserVersion")).and_then(Value::as_u64) != Some(OPENAI_PRICING_PARSER_VERSION) {
                // 12시간 갱신 주기를 기다리지 않고 검증된 새 해석기로 다시 받는다.
                // 실패해도 구형 값을 설치하지 않아 잘못된 표시 비용을 만들지 않는다.
                self.refresh_provider("codex", true).await?;
                continue;
            }
            if let Some(catalog) = catalog.filter(|catalog| js_truthy(Some(catalog))) {
                self.options.catalogs.install(&provider, &catalog)?;
            }
        }
        Ok(self.status())
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

    /// 두 공급자를 동시에 갱신한다. Promise.all처럼 먼저 실패한 쪽 오류를 돌려주고 남은 갱신은 그대로 진행된다.
    pub async fn refresh_due(self: &Arc<Self>, force: bool) -> Result<Vec<Value>, String> {
        let mut pending: FuturesUnordered<_> = PRICING_CATALOG_SOURCES
            .iter()
            .enumerate()
            .map(|(index, source)| {
                let service = Arc::clone(self);
                let task = tokio::spawn(async move { service.refresh_provider(source.provider, force).await });
                async move { (index, task.await) }
            })
            .collect();
        let mut results = vec![Value::Null; PRICING_CATALOG_SOURCES.len()];
        while let Some((index, finished)) = pending.next().await {
            results[index] = finished.map_err(|error| error.to_string())??;
        }
        Ok(results)
    }

    pub async fn refresh_provider(&self, provider: &str, force: bool) -> Result<Value, String> {
        let Some(source) = pricing_source(provider) else {
            return Err(format!("지원하지 않는 가격 제공자입니다: {provider}"));
        };
        let claim = RefreshClaim {
            provider: source.provider,
            source_url: source.url,
            attempted_at: (self.options.now)(),
            minimum_interval_milliseconds: self.options.refresh_interval_milliseconds,
            force,
        };
        let Some(claimed) = self.store.claim_refresh(claim).await? else {
            return Ok(json!({ "provider": source.provider, "status": "fresh" }));
        };
        self.set_row(claimed.clone());
        match self.refresh_claimed(source, &claimed).await {
            Ok(model_count) => Ok(json!({ "provider": source.provider, "status": "updated", "modelCount": model_count })),
            Err(message) => {
                let saved = self.store.save_failure(source.provider, &message).await?;
                self.set_row(saved.unwrap_or(PricingCatalogRow { last_error: Some(message.clone()), ..claimed }));
                (self.options.warn)(
                    &format!("{} 공식 가격표를 갱신하지 못해 마지막 정상값을 유지합니다.", source.provider),
                    &message,
                );
                Ok(json!({ "provider": source.provider, "status": "failed", "error": message }))
            }
        }
    }

    /// 받은 가격표를 저장값(없으면 지금 단가표)과 합쳐 저장하고 단가표에 올린다. Node try 블록과 같은 범위다.
    async fn refresh_claimed(&self, source: &PricingSource, claimed: &PricingCatalogRow) -> Result<usize, String> {
        let body = self.fetch_source(source.url).await?;
        let parsed = (source.parse)(&body)?;
        let stored = self.row(source.provider).and_then(|row| row.catalog).filter(|catalog| !catalog.is_null());
        let base = stored
            .map_or_else(|| self.options.catalogs.current(source.provider), |catalog| normalized_catalog(&catalog));
        let merged = merge_catalogs(&base, &parsed);
        let fetched_at = (self.options.now)();
        let mut merged_json = merged.to_json();
        if source.provider == "codex" {
            merged_json["openaiParserVersion"] = Value::from(OPENAI_PRICING_PARSER_VERSION);
        }
        let saved = self.store.save_success(source.provider, &merged_json, fetched_at).await?;
        self.set_row(saved.unwrap_or_else(|| PricingCatalogRow {
            catalog: Some(merged_json),
            fetched_at: Some(fetched_at),
            last_error: None,
            ..claimed.clone()
        }));
        self.options.catalogs.install_catalog(source.provider, &merged)?;
        Ok(merged.models.len())
    }

    pub fn status(&self) -> Vec<Value> {
        let rows = self.rows.lock().unwrap_or_else(PoisonError::into_inner);
        let time = |value: Option<f64>| value.map_or(Value::Null, |milliseconds| Value::from(iso_string(milliseconds)));
        PRICING_CATALOG_SOURCES
            .iter()
            .map(|source| {
                let row = rows.get(source.provider);
                json!({
                    "provider": source.provider,
                    "sourceURL": source.url,
                    "fetchedAt": time(row.and_then(|row| row.fetched_at)),
                    "lastAttemptedAt": time(row.and_then(|row| row.last_attempted_at)),
                    "lastError": row.and_then(|row| row.last_error.clone()),
                    "modelCount": self.options.catalogs.current(source.provider).models.len(),
                })
            })
            .collect()
    }

    async fn run_cycle(self: Arc<Self>) {
        if self.stopped.load(Ordering::SeqCst) || self.running.swap(true, Ordering::SeqCst) {
            return;
        }
        if let Err(message) = self.refresh_due(false).await {
            (self.options.warn)("공식 가격표 갱신 주기를 완료하지 못했습니다.", &message);
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

    /// 가장 이른 공급자의 마지막 시도 + 주기까지 남은 밀리초. 시도 기록이 없으면 지금이고 최소 1초다.
    fn next_cycle_delay(&self) -> f64 {
        let now = (self.options.now)();
        let interval = self.options.refresh_interval_milliseconds;
        let rows = self.rows.lock().unwrap_or_else(PoisonError::into_inner);
        let next = PRICING_CATALOG_SOURCES
            .iter()
            .map(|source| match rows.get(source.provider).and_then(|row| row.last_attempted_at) {
                // Node는 Date.parse(Date)로 읽어 Date.toString의 초 단위로 떨어진다.
                Some(attempted) => (attempted / 1_000.0).floor() * 1_000.0 + interval,
                None => now,
            })
            .fold(f64::INFINITY, f64::min);
        js_max(1_000.0, next - now)
    }

    async fn fetch_source(&self, url: &str) -> Result<String, String> {
        let page = self.fetcher.fetch(url, self.options.request_timeout).await?;
        if !(200..=299).contains(&page.status) {
            return Err(format!("가격표 HTTP {}", page.status));
        }
        if page.body.len() > MAX_SOURCE_BYTES {
            return Err("가격표 응답이 허용 크기를 넘었습니다.".into());
        }
        Ok(page.body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const OPENAI_FIXTURE: &str = "
# Pricing

### Standard pricing data

| Model | Short context input | Short context cached input | Short context cache writes | Short context output | Long context input | Long context cached input | Long context cache writes | Long context output |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| gpt-new | $3.00 | $0.30 | $3.75 | $15.00 | $6.00 | $0.60 | $7.50 | $22.50 |
| gpt-old (<272K context length) | $1.00 | - | - | $5.00 | - | - | - | - |

### Fast pricing data

| Model | Short context input | Short context cached input | Short context cache writes | Short context output | Long context input | Long context cached input | Long context cache writes | Long context output |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| gpt-new | $6.00 | $0.60 | $7.50 | $30.00 | $12.00 | $1.20 | $15.00 | $45.00 |
";

    const GEMINI_FIXTURE: &str = r#"
<h2 id="gemini-new-flash">Gemini New Flash</h2>
<em><code>gemini-new-flash</code></em>
<section><h3 data-text="Standard">Standard</h3><table>
<tr><th></th><th>Free Tier</th><th>Paid Tier</th></tr>
<tr><td>Input price</td><td>Free</td><td>$0.75 through December 31, 2026.<br>$1.50 starting January 1, 2027.</td></tr>
<tr><td>Output price (including thinking tokens)</td><td>Free</td><td>$3.75 through December 31, 2026.<br>$7.50 starting January 1, 2027.</td></tr>
<tr><td>Context caching price</td><td>Free</td><td>$0.075 through December 31, 2026.<br>$0.15 starting January 1, 2027.<br>$1.00 / 1,000,000 tokens per hour (storage price)</td></tr>
</table></section>
<h2 id="gemini-new-pro-preview">Gemini New Pro</h2>
<em><code>gemini-new-pro-preview</code></em>
<section><h3 data-text="Standard">Standard</h3><table>
<tr><td>Input price</td><td>Not available</td><td>$2.00, prompts <= 200k tokens<br>$4.00, prompts > 200k tokens</td></tr>
<tr><td>Output price (including thinking tokens)</td><td>Not available</td><td>$12.00, prompts <= 200k tokens<br>$18.00, prompts > 200k</td></tr>
<tr><td>Context caching price</td><td>Not available</td><td>$0.20, prompts <= 200k tokens<br>$0.40, prompts > 200k</td></tr>
</table></section>
"#;

    fn rate(input: f64, cached: Option<f64>, cache_write: Option<f64>, output: f64) -> PricingRate {
        PricingRate { input, cached, cache_write, output }
    }

    fn at(text: &str) -> f64 {
        date_parse(text).expect("ISO 시각")
    }

    fn standard<'a>(catalog: &'a PricingCatalog, model: &str) -> &'a [PricingRule] {
        catalog.get(model).and_then(|modes| modes.standard.as_deref()).expect("standard 규칙")
    }

    #[test]
    fn openai_current_columns_and_repeated_sections_do_not_swap_prices() {
        // 2026-11-03 공식 pricing.md의 열 순서. 12h 열 때문에 long 시작 위치도 달라졌다.
        let source = r#"
### Standard pricing data
| Model | Short context input | Short context output | Short context cached input | Short context 30m write | Short context 12h write | Long context input | Long context output | Long context cached input | Long context 30m write | Long context 12h write |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| gpt-6.1-sol | $2 | $10 | $0.10 | $2.50 | $5 | $4 | $15 | $0.20 | $5 | $10 |
### Fast pricing data
| Model | Short context input | Short context output | Short context cached input | Short context 30m write | Short context 12h write | Long context input | Long context output | Long context cached input | Long context 30m write | Long context 12h write |
| gpt-6.1-sol | $4 | $20 | $0.20 | $5 | $10 | $8 | $30 | $0.40 | $10 | $20 |
### Standard pricing data
| Model | Short context input | Short context output | Short context cached input | Short context 30m write | Short context 12h write | Long context input | Long context output | Long context cached input | Long context 30m write | Long context 12h write |
| gpt-other | $1 | $5 | $0.05 | $1.25 | $2.50 | $2 | $7.50 | $0.10 | $2.50 | $5 |
"#;
        let parsed = parse_openai_pricing_markdown(source).unwrap();
        assert_eq!(standard(&parsed, "gpt-6.1-sol")[0], PricingRule {
            max_prompt_tokens: Some(272_000.0), ..PricingRule::rate(2.0, Some(0.1), Some(2.5), 10.0)
        });
        assert_eq!(standard(&parsed, "gpt-6.1-sol")[1], PricingRule {
            min_prompt_tokens: Some(272_001.0), ..PricingRule::rate(4.0, Some(0.2), Some(5.0), 15.0)
        });
        let fast = parsed.get("gpt-6.1-sol").unwrap().fast.as_ref().unwrap();
        assert_eq!(fast[0].cached, Some(0.2));
        assert_eq!(fast[1].output, 30.0);
        assert!(parsed.get("gpt-other").is_some());
        let catalogs = PricingCatalogs::new();
        catalogs.install_catalog("codex", &parsed).unwrap();
        let usage = json!({ "inputTokens": 1_537_309, "cachedInputTokens": 1_529_600, "outputTokens": 8_533 });
        // 7개의 짧은 요청 합계: 입력 누적을 장문 요청으로 간주하지 않는다.
        let request = json!({ "inputTokens": 219_615, "cachedInputTokens": 218_514, "outputTokens": 1_219 });
        let mut requests = vec![json!({ "usage": request }); 6];
        requests.push(json!({ "usage": { "inputTokens": 219_619, "cachedInputTokens": 218_516, "outputTokens": 1_219 } }));
        let mut usage = usage;
        usage["requestUsages"] = json!(requests);
        assert_eq!(crate::token_cost_estimator::estimate_turn_token_cost(&catalogs, &crate::token_cost_estimator::CostQuery {
            backend: Some("codex"), model: Some("gpt-6.1-sol"), usage: Some(&usage), ..Default::default()
        }), Some(0.253708));
        assert!(parse_openai_pricing_markdown(&source.replace("Short context output", "Unknown column")).is_err());
    }

    #[test]
    fn openai_가격표에서_standard_fast와_장문_단가를_수집한다() {
        let parsed = parse_openai_pricing_markdown(OPENAI_FIXTURE).unwrap();
        assert_eq!(standard(&parsed, "gpt-old"), [PricingRule::rate(1.0, None, None, 5.0)]);
        assert_eq!(
            standard(&parsed, "gpt-new")[1],
            PricingRule { min_prompt_tokens: Some(272_001.0), ..PricingRule::rate(6.0, Some(0.6), Some(7.5), 22.5) }
        );
        assert_eq!(parsed.get("gpt-new").unwrap().fast.as_ref().unwrap()[0].output, 30.0);
        let catalogs = PricingCatalogs::new();
        catalogs.install("codex", &parsed.to_json()).unwrap();
        let query = RateQuery {
            backend: Some("codex"),
            model: Some("gpt-new"),
            fast_mode: true,
            prompt_tokens: 300_000.0,
            priced_at: None,
        };
        assert_eq!(catalogs.rate_for(&query), Some(rate(12.0, Some(1.2), Some(15.0), 45.0)));
        assert_eq!(rate(12.0, Some(1.2), Some(15.0), 45.0).to_json().to_string(), r#"{"input":12.0,"cached":1.2,"cacheWrite":15.0,"output":45.0}"#);
    }

    #[test]
    fn gemini_가격표에서_기간_20만_토큰_경계와_preview_별칭을_수집한다() {
        let parsed = parse_gemini_pricing_html(GEMINI_FIXTURE).unwrap();
        let names: Vec<&str> = parsed.models.iter().map(|(name, _)| name.as_str()).collect();
        assert_eq!(names, ["gemini-new-flash", "gemini-new-pro-preview", "gemini-new-pro"]);
        assert_eq!(
            standard(&parsed, "gemini-new-flash"),
            [
                PricingRule {
                    effective_until: Some("2027-01-01T00:00:00.000Z".into()),
                    ..PricingRule::rate(0.75, Some(0.075), None, 3.75)
                },
                PricingRule {
                    effective_from: Some("2027-01-01T00:00:00.000Z".into()),
                    ..PricingRule::rate(1.5, Some(0.15), None, 7.5)
                },
            ]
        );
        let catalogs = PricingCatalogs::new();
        catalogs.install("antigravity", &parsed.to_json()).unwrap();
        let query = |model, prompt_tokens, priced_at| RateQuery {
            backend: Some("antigravity"),
            model: Some(model),
            prompt_tokens,
            priced_at,
            ..RateQuery::default()
        };
        assert_eq!(
            catalogs.rate_for(&query("gemini-new-flash", 10.0, Some(at("2026-12-31T12:00:00Z")))),
            Some(rate(0.75, Some(0.075), None, 3.75))
        );
        assert_eq!(
            catalogs.rate_for(&query("gemini-new-flash", 10.0, Some(at("2027-01-01T00:00:00Z")))).unwrap().output,
            7.5
        );
        assert_eq!(catalogs.rate_for(&query("gemini-new-pro", 200_001.0, None)).unwrap().output, 18.0);
        assert_eq!(catalogs.rate_for(&query("gemini-new-pro", 200_000.0, None)).unwrap().output, 12.0);
    }

    #[test]
    fn 새_카탈로그는_사라진_구모델의_마지막_정상_단가를_보존한다() {
        let merged = merge_pricing_catalog(
            &json!({ "version": 1, "models": { "legacy": { "standard": [{ "input": 1, "output": 2 }] } } }),
            &json!({ "version": 1, "models": { "current": { "standard": [{ "input": 3, "output": 4 }] } } }),
        );
        assert_eq!(standard(&merged, "legacy")[0].output, 2.0);
        assert_eq!(standard(&merged, "current")[0].output, 4.0);
        // 모델 안에서는 새 쪽에 있는 방식만 바뀐다.
        let both = json!({ "models": { "gpt-x": { "standard": [{ "input": 1, "output": 2 }], "fast": [{ "input": 5, "output": 6 }] } } });
        let update = json!({ "models": { "gpt-x": { "standard": [{ "input": 3, "output": 4 }] } } });
        let merged = merge_pricing_catalog(&both, &update);
        assert_eq!(standard(&merged, "gpt-x")[0].input, 3.0);
        assert_eq!(merged.get("gpt-x").unwrap().fast.as_ref().unwrap()[0].input, 5.0);
    }

    #[test]
    fn 내장_단가는_27만_2천_토큰과_2027년_경계에서_바뀐다() {
        let catalogs = PricingCatalogs::new();
        let codex = |model, fast_mode, prompt_tokens| {
            catalogs.rate_for(&RateQuery { backend: Some("codex"), model: Some(model), fast_mode, prompt_tokens, priced_at: None })
        };
        assert_eq!(codex("gpt-5.6", false, 272_000.0), Some(rate(4.0, Some(0.4), Some(5.0), 20.0)));
        assert_eq!(codex("gpt-5.6", false, 272_001.0), Some(rate(8.0, Some(0.8), Some(10.0), 30.0)));
        assert_eq!(codex("gpt-5.6", true, 272_001.0), Some(rate(16.0, Some(1.6), Some(20.0), 60.0)));
        assert_eq!(codex("gpt-5.6-luna", false, f64::NAN), Some(rate(0.2, Some(0.02), Some(0.25), 1.2)));
        assert_eq!(codex("gpt-5.6-luna", false, -5.0), Some(rate(0.2, Some(0.02), Some(0.25), 1.2)));
        assert_eq!(codex("gpt-unknown", false, 0.0), None);
        let gemini = |model, fast_mode, priced_at| {
            catalogs.rate_for(&RateQuery {
                backend: Some("antigravity"),
                model: Some(model),
                fast_mode,
                prompt_tokens: 0.0,
                priced_at: Some(priced_at),
            })
        };
        assert_eq!(gemini("gemini-3.8-flash", false, at("2026-12-31T23:59:59.999Z")).unwrap().input, 0.75);
        assert_eq!(gemini("gemini-3.8-flash", true, at("2027-01-01T00:00:00.000Z")).unwrap().input, 1.5);
        assert_eq!(gemini("gemini-3.5-flash", false, 0.0), Some(rate(1.5, Some(0.15), None, 9.0)));
        let other = |backend| catalogs.rate_for(&RateQuery { backend, model: Some("gpt-5.6"), ..RateQuery::default() });
        assert_eq!(other(Some("claude")), None);
        assert_eq!(other(None), None);
        assert_eq!(catalogs.rate_for(&RateQuery { backend: Some("codex"), ..RateQuery::default() }), None);
        assert_eq!(catalogs.install("claude", &json!({})).unwrap_err(), "지원하지 않는 가격 제공자입니다: claude");
        assert_eq!(catalogs.current("claude"), PricingCatalog::default());
        assert_eq!(catalogs.current("codex").models.len(), 5);
    }

    #[test]
    fn 저장값_정규화는_js_number와_date_parse_규칙을_따른다() {
        let catalog = normalized_catalog(&json!({ "models": {
            "ok-model": {
                "standard": [
                    {
                        "input": "3", "output": true, "cached": "", "cacheWrite": null,
                        "minPromptTokens": null, "maxPromptTokens": "12.5",
                        "effectiveFrom": 2027, "effectiveUntil": "bad"
                    },
                    { "input": 1 },
                    { "input": -1, "output": 2 },
                    { "input": 10_001, "output": 2 },
                    [1, 2]
                ],
                "fast": "x"
            },
            "Bad-Model": { "standard": [{ "input": 1, "output": 2 }] },
            "x": { "standard": [{ "input": 1, "output": 2 }] },
            "empty-model": { "standard": [] },
            "array-model": [1]
        } }));
        assert_eq!(catalog.models.len(), 1);
        assert_eq!(
            standard(&catalog, "ok-model"),
            [PricingRule {
                min_prompt_tokens: Some(0.0),
                max_prompt_tokens: Some(13.0),
                effective_from: Some("2027-01-01T00:00:00.000Z".into()),
                ..PricingRule::rate(3.0, None, None, 1.0)
            }]
        );
        assert_eq!(normalized_catalog(&json!([1])), PricingCatalog::default());
        assert_eq!(normalized_catalog(&json!({ "models": [] })), PricingCatalog::default());
    }

    #[test]
    fn 마크다운_절은_다음_세_샵_제목_줄_앞에서_끝난다() {
        let text = "### Standard pricing data\n| a |\n####x\n| b |\r### Next\n| c |";
        assert_eq!(markdown_section(text, "### Standard pricing data"), "\n| a |\n####x\n| b |\r");
        assert_eq!(markdown_section("x", "### Standard pricing data"), "");
        assert_eq!(markdown_section("### Fast pricing data###\tz", "### Fast pricing data"), "");
        assert_eq!(markdown_cells(" | a | b |c| "), ["a", "b", "c"]);
        assert_eq!(markdown_cells("|"), [""]);
        assert_eq!(normalize_openai_model_id(" **GPT_X** (<1M ctx) "), "gptx");
        assert_eq!(markdown_price("$1,250.5"), Some(1250.5));
        assert_eq!(markdown_price("$5. per"), Some(5.0));
        assert_eq!(markdown_price("$$5"), None);
        assert_eq!(markdown_price("-"), None);
        assert_eq!(parse_openai_pricing_markdown("none").unwrap_err(), "OpenAI 가격표에서 모델 단가를 찾지 못했습니다.");
        assert_eq!(parse_gemini_pricing_html("<h2 id=\"x\">").unwrap_err(), "Gemini 가격표에서 모델 단가를 찾지 못했습니다.");
    }

    #[test]
    fn 가격표_날짜는_v8_옛_해석_규칙을_따른다() {
        let date = |value| month_day_year_utc(value).map(iso_string);
        assert_eq!(date("December 31, 2026").as_deref(), Some("2026-12-31T00:00:00.000Z"));
        assert_eq!(date("decemberx 1, 2027").as_deref(), Some("2027-12-01T00:00:00.000Z"));
        assert_eq!(date("February 30, 2026").as_deref(), Some("2026-03-02T00:00:00.000Z"));
        assert_eq!(date("June 45, 2026"), None);
        assert_eq!(date("Ju 5, 2026"), None);
        assert_eq!(date("Foo 5, 0012").as_deref(), Some("2001-05-12T00:00:00.000Z"));
        assert_eq!(date("Foo 45, 0005").as_deref(), Some("2045-05-01T00:00:00.000Z"));
        assert_eq!(date("March 7, 0049").as_deref(), Some("2049-03-07T00:00:00.000Z"));
        assert_eq!(date("March 7, 0099").as_deref(), Some("1999-03-07T00:00:00.000Z"));
        assert_eq!(date("March 0, 2026"), None);
        assert_eq!(iso_string(month_day_year_utc("December 31, 9999").unwrap() + 86_400_000.0), "+010000-01-01T00:00:00.000Z");
        assert_eq!(html_text(" <i>Input</i>\u{a0}price&amp;lt; a\u{85}b "), "Input price&lt; a\u{85}b");
        assert_eq!(scaled_token_count("1.5", "M"), Some(1_500_000.0));
        assert_eq!(scaled_token_count("1.2.3", "k"), None);
        assert_eq!(utf16_prefix(&format!("{}😀", "a".repeat(999)), 1_000), format!("{}\u{fffd}", "a".repeat(999)));
    }

    #[derive(Default)]
    struct MemoryStore {
        rows: Mutex<HashMap<String, PricingCatalogRow>>,
    }

    impl MemoryStore {
        fn row(&self, provider: &str) -> PricingCatalogRow {
            self.rows.lock().unwrap()[provider].clone()
        }
    }

    impl PricingCatalogStore for MemoryStore {
        async fn load(&self) -> Result<Vec<PricingCatalogRow>, String> {
            Ok(self.rows.lock().unwrap().values().cloned().collect())
        }

        async fn claim_refresh(&self, claim: RefreshClaim<'_>) -> Result<Option<PricingCatalogRow>, String> {
            let mut rows = self.rows.lock().unwrap();
            let previous = rows.get(claim.provider).cloned().unwrap_or_default();
            let last = previous.last_attempted_at.unwrap_or(f64::NEG_INFINITY);
            if !claim.force && claim.attempted_at - last < claim.minimum_interval_milliseconds {
                return Ok(None);
            }
            let next = PricingCatalogRow {
                provider: claim.provider.into(),
                source_url: claim.source_url.into(),
                last_attempted_at: Some(claim.attempted_at),
                ..previous
            };
            rows.insert(claim.provider.into(), next.clone());
            Ok(Some(next))
        }

        async fn save_success(
            &self,
            provider: &str,
            catalog: &Value,
            fetched_at: f64,
        ) -> Result<Option<PricingCatalogRow>, String> {
            let mut rows = self.rows.lock().unwrap();
            let row = rows.get_mut(provider).unwrap();
            row.catalog = Some(catalog.clone());
            row.fetched_at = Some(fetched_at);
            row.last_error = None;
            Ok(Some(row.clone()))
        }

        async fn save_failure(&self, provider: &str, error: &str) -> Result<Option<PricingCatalogRow>, String> {
            let mut rows = self.rows.lock().unwrap();
            let row = rows.get_mut(provider).unwrap();
            row.last_error = Some(error.into());
            Ok(Some(row.clone()))
        }
    }

    #[derive(Default)]
    struct FakeFetcher {
        fetches: Mutex<Vec<String>>,
        openai_offline: AtomicBool,
        replacement: Mutex<Option<(u16, String)>>,
    }

    impl FakeFetcher {
        fn count(&self) -> usize {
            self.fetches.lock().unwrap().len()
        }
    }

    impl PricingFetcher for FakeFetcher {
        async fn fetch(&self, url: &str, _timeout: Duration) -> Result<FetchedPage, String> {
            self.fetches.lock().unwrap().push(url.into());
            if url.contains("openai") && self.openai_offline.load(Ordering::SeqCst) {
                return Err("offline".into());
            }
            if let Some((status, body)) = self.replacement.lock().unwrap().clone() {
                return Ok(FetchedPage { status, body });
            }
            let body = if url.contains("openai") { OPENAI_FIXTURE } else { GEMINI_FIXTURE };
            Ok(FetchedPage { status: 200, body: body.into() })
        }
    }

    type TestService = Arc<PricingCatalogService<MemoryStore, FakeFetcher>>;

    fn test_service(clock: &Arc<Mutex<f64>>, warnings: &Arc<Mutex<Vec<String>>>) -> TestService {
        let (clock, warnings) = (Arc::clone(clock), Arc::clone(warnings));
        PricingCatalogService::new(
            MemoryStore::default(),
            FakeFetcher::default(),
            PricingServiceOptions {
                catalogs: Arc::new(PricingCatalogs::new()),
                now: Arc::new(move || *clock.lock().unwrap()),
                warn: Arc::new(move |message, detail| warnings.lock().unwrap().push(format!("{message} {detail}"))),
                ..PricingServiceOptions::default()
            },
        )
    }

    fn advance(clock: &Mutex<f64>, milliseconds: f64) {
        *clock.lock().unwrap() += milliseconds;
    }

    const HOUR: f64 = 60.0 * 60.0 * 1_000.0;

    #[tokio::test]
    async fn old_openai_cache_is_refetched_on_restart_even_before_twelve_hours() {
        let clock = Arc::new(Mutex::new(at("2026-11-03T00:00:00Z")));
        let service = test_service(&clock, &Arc::default());
        service.refresh_due(false).await.unwrap();
        let mut rows = service.store().rows.lock().unwrap().clone();
        let catalog = rows.get_mut("codex").unwrap().catalog.as_mut().unwrap();
        catalog.as_object_mut().unwrap().remove("openaiParserVersion");
        catalog["models"]["gpt-new"]["standard"][0]["cached"] = json!(15);
        advance(&clock, HOUR);
        let restarted = PricingCatalogService::new(
            MemoryStore { rows: Mutex::new(rows.clone()) },
            FakeFetcher::default(),
            PricingServiceOptions { catalogs: Arc::new(PricingCatalogs::new()), ..service.options.clone() },
        );
        restarted.load_cached().await.unwrap();
        assert_eq!(restarted.fetcher().count(), 1);
        assert_eq!(standard(&restarted.options.catalogs.current("codex"), "gpt-new")[0].cached, Some(0.3));
        assert_eq!(restarted.row("codex").unwrap().catalog.unwrap()["openaiParserVersion"], json!(OPENAI_PRICING_PARSER_VERSION));

        let offline = PricingCatalogService::new(
            MemoryStore { rows: Mutex::new(rows) },
            FakeFetcher::default(),
            PricingServiceOptions { catalogs: Arc::new(PricingCatalogs::new()), ..service.options.clone() },
        );
        offline.fetcher().openai_offline.store(true, Ordering::SeqCst);
        offline.load_cached().await.unwrap();
        assert_eq!(offline.fetcher().count(), 1);
        assert!(offline.options.catalogs.current("codex").get("gpt-new").is_none());
    }

    #[tokio::test]
    async fn 자동_갱신은_재시작과_무관하게_공급자별_12시간에_한_번만_시도한다() {
        let clock = Arc::new(Mutex::new(at("2026-09-05T00:00:00Z")));
        let service = test_service(&clock, &Arc::default());
        service.load_cached().await.unwrap();
        assert_eq!(service.next_cycle_delay(), 1_000.0);
        let results = service.refresh_due(false).await.unwrap();
        assert_eq!(
            results,
            [
                json!({ "provider": "codex", "status": "updated", "modelCount": 7 }),
                json!({ "provider": "antigravity", "status": "updated", "modelCount": 9 }),
            ]
        );
        assert_eq!(service.fetcher().count(), 2);

        advance(&clock, 11.0 * HOUR);
        let results = service.refresh_due(false).await.unwrap();
        assert_eq!(results[0], json!({ "provider": "codex", "status": "fresh" }));
        assert_eq!(service.fetcher().count(), 2);
        assert_eq!(service.next_cycle_delay(), HOUR);

        advance(&clock, HOUR);
        service.refresh_due(false).await.unwrap();
        assert_eq!(service.fetcher().count(), 4);
        service.refresh_due(true).await.unwrap();
        assert_eq!(service.fetcher().count(), 6);

        // 시도 기록이 저장소에 있으니 새 서비스(재시작)도 같은 간격을 지킨다.
        let restarted = PricingCatalogService::new(
            MemoryStore { rows: Mutex::new(service.store().rows.lock().unwrap().clone()) },
            FakeFetcher::default(),
            PricingServiceOptions { catalogs: Arc::new(PricingCatalogs::new()), ..service.options.clone() },
        );
        let status = restarted.load_cached().await.unwrap();
        assert_eq!(status[0]["modelCount"], json!(7));
        assert_eq!(status[0]["lastAttemptedAt"], json!("2026-09-05T12:00:00.000Z"));
        restarted.refresh_due(false).await.unwrap();
        assert_eq!(restarted.fetcher().count(), 0);
    }

    #[tokio::test]
    async fn 공식_가격표_조회_실패는_마지막_정상_카탈로그를_덮어쓰지_않는다() {
        let clock = Arc::new(Mutex::new(at("2026-09-05T00:00:00Z")));
        let warnings = Arc::new(Mutex::new(Vec::new()));
        let service = test_service(&clock, &warnings);
        service.load_cached().await.unwrap();
        service.refresh_due(false).await.unwrap();
        assert!(service.options.catalogs.current("codex").get("gpt-new").is_some());

        service.fetcher().openai_offline.store(true, Ordering::SeqCst);
        advance(&clock, 12.0 * HOUR);
        let results = service.refresh_due(false).await.unwrap();
        assert_eq!(results[0], json!({ "provider": "codex", "status": "failed", "error": "offline" }));
        assert!(service.options.catalogs.current("codex").get("gpt-new").is_some());
        assert_eq!(service.store().row("codex").last_error.as_deref(), Some("offline"));
        assert!(service.store().row("codex").catalog.is_some());
        assert_eq!(*warnings.lock().unwrap(), ["codex 공식 가격표를 갱신하지 못해 마지막 정상값을 유지합니다. offline"]);
    }

    #[tokio::test]
    async fn 응답_상태_크기_해석_실패는_node와_같은_문구로_남긴다() {
        let clock = Arc::new(Mutex::new(at("2026-09-05T00:00:00Z")));
        let service = test_service(&clock, &Arc::default());
        let failure = |status: u16, body: String| {
            *service.fetcher().replacement.lock().unwrap() = Some((status, body));
        };
        failure(503, String::new());
        let result = service.refresh_provider("codex", true).await.unwrap();
        assert_eq!(result, json!({ "provider": "codex", "status": "failed", "error": "가격표 HTTP 503" }));
        failure(200, "a".repeat(MAX_SOURCE_BYTES + 1));
        let result = service.refresh_provider("codex", true).await.unwrap();
        assert_eq!(result["error"], json!("가격표 응답이 허용 크기를 넘었습니다."));
        failure(200, "a".repeat(MAX_SOURCE_BYTES));
        let result = service.refresh_provider("codex", true).await.unwrap();
        assert_eq!(result["error"], json!("OpenAI 가격표에서 모델 단가를 찾지 못했습니다."));
        assert_eq!(service.refresh_provider("claude", true).await.unwrap_err(), "지원하지 않는 가격 제공자입니다: claude");
        assert_eq!(service.store().row("codex").catalog, None);
    }
}
