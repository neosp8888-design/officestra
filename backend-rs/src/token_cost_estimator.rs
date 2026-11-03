// CLI가 보고한 토큰 사용량을 공식 API 단가 기준 USD 추정 비용으로 바꾼다. Node token-cost-estimator.mjs를 옮겼다.
// Claude는 예외로, Claude Code가 보고한 금액만 쓰고 토큰으로 다시 계산하지 않는다.

use serde_json::Value;

use crate::js::{date_parse, js_max, js_min, js_string, js_to_number, js_truthy, string_to_number};
use crate::pricing_catalog::{PricingCatalogs, RateQuery};

const TOKEN_FIELDS: [&str; 4] = ["inputTokens", "outputTokens", "cachedInputTokens", "cacheWriteInputTokens"];

/// estimateTurnTokenCost·estimateTokenCost 인자. 기준 시각은 밀리초이고 없으면 지금이다.
#[derive(Clone, Copy, Debug, Default)]
pub struct CostQuery<'a> {
    pub backend: Option<&'a str>,
    pub model: Option<&'a str>,
    pub fast_mode: bool,
    pub usage: Option<&'a Value>,
    pub priced_at: Option<f64>,
}

/// 한 턴에는 모델 요청이 여럿 있으므로 턴 누적 입력을 한 요청의 문맥 길이로 쓰지 않는다. Claude는 보고 금액만 쓴다.
pub fn estimate_turn_token_cost(catalogs: &PricingCatalogs, query: &CostQuery<'_>) -> Option<f64> {
    let usage = match query.usage {
        Some(usage) if js_truthy(Some(usage)) && query.backend != Some("claude") => usage,
        _ => return estimate_token_cost(catalogs, query),
    };
    if let Some(requests) = usage.get("requestUsages").and_then(Value::as_array).filter(|requests| !requests.is_empty()) {
        return estimate_request_costs(catalogs, query, usage, requests);
    }
    // 요청 기록이 없으면 문맥 길이와 무관한 단가만 확실하다. 가능한 길이 양끝 단가가 다르면 추측하지 않는다.
    let cached = if query.backend == Some("antigravity") { defined(usage.get("cachedInputTokens")) } else { None };
    let longest = js_sum_number(defined(usage.get("inputTokens")), cached);
    let rate = |prompt_tokens| {
        catalogs.rate_for(&RateQuery {
            backend: query.backend,
            model: query.model,
            fast_mode: query.fast_mode,
            prompt_tokens,
            priced_at: query.priced_at,
        })
    };
    if rate(0.0) != rate(longest) {
        return None;
    }
    estimate_token_cost(catalogs, query)
}

/// estimateTokenCost. 사용량이 없거나 단가를 모르면 비용을 만들지 않는다.
pub fn estimate_token_cost(catalogs: &PricingCatalogs, query: &CostQuery<'_>) -> Option<f64> {
    let usage = query.usage.filter(|usage| js_truthy(Some(usage)))?;
    match query.backend {
        Some("claude") => reported_claude_cost(usage),
        Some("antigravity") => estimate_antigravity_cost(catalogs, query, usage),
        Some("codex") => estimate_codex_cost(catalogs, query, usage),
        _ => None,
    }
}

/// 요청별 사용량 합이 턴 합계와 칸마다 같을 때만 요청마다 따로 단가를 매겨 더한다.
fn estimate_request_costs(catalogs: &PricingCatalogs, query: &CostQuery<'_>, usage: &Value, requests: &[Value]) -> Option<f64> {
    let request_usages: Vec<&Value> =
        requests.iter().map(|request| request.get("usage").filter(|usage| js_truthy(Some(usage)))).collect::<Option<_>>()?;
    for field in TOKEN_FIELDS {
        let turn_total = counted_tokens(usage.get(field))?;
        let mut total = 0.0;
        for request_usage in &request_usages {
            total += counted_tokens(request_usage.get(field))?;
        }
        if total != turn_total {
            return None;
        }
    }
    let mut total = 0.0;
    for (request, request_usage) in requests.iter().zip(request_usages) {
        let model = defined(request.get("model")).map(js_string);
        let amount = estimate_token_cost(
            catalogs,
            &CostQuery {
                backend: query.backend,
                model: model.as_deref().or(query.model),
                fast_mode: defined(request.get("fastMode")).map_or(query.fast_mode, |fast| js_truthy(Some(fast))),
                usage: Some(request_usage),
                priced_at: defined(request.get("pricedAt")).map_or(query.priced_at, |at| Some(price_timestamp(at))),
            },
        )?;
        total += amount;
    }
    Some(rounded_cost(total))
}

fn estimate_codex_cost(catalogs: &PricingCatalogs, query: &CostQuery<'_>, usage: &Value) -> Option<f64> {
    let input = nonnegative_number(usage.get("inputTokens"))?;
    let output = nonnegative_number(usage.get("outputTokens"))?;
    let prices = catalogs.rate_for(&RateQuery {
        backend: Some("codex"),
        model: query.model,
        fast_mode: query.fast_mode,
        prompt_tokens: input,
        priced_at: query.priced_at,
    })?;
    let cached = js_min(input, nonnegative_number(usage.get("cachedInputTokens")).unwrap_or(0.0));
    let cache_write =
        js_min(js_max(input - cached, 0.0), nonnegative_number(usage.get("cacheWriteInputTokens")).unwrap_or(0.0));
    let uncached = js_max(input - cached - cache_write, 0.0);
    let cost = (uncached * prices.input
        + cached * prices.cached.unwrap_or(prices.input)
        + cache_write * prices.cache_write.unwrap_or(prices.input)
        + output * prices.output)
        / 1_000_000.0;
    Some(rounded_cost(cost))
}

/// agy stream-json은 캐시 미스 입력과 cache_read_tokens를 따로 보고하고, output_tokens에 thinking_tokens가 이미 들어 있다.
/// 컨텍스트 캐시 보관 시간은 턴 데이터로 알 수 없어 저장 요금은 뺀다.
fn estimate_antigravity_cost(catalogs: &PricingCatalogs, query: &CostQuery<'_>, usage: &Value) -> Option<f64> {
    let input = nonnegative_number(usage.get("inputTokens"))?;
    let output = nonnegative_number(usage.get("outputTokens"))?;
    let cached = nonnegative_number(usage.get("cachedInputTokens")).unwrap_or(0.0);
    let prices = catalogs.rate_for(&RateQuery {
        backend: Some("antigravity"),
        model: query.model,
        fast_mode: false,
        prompt_tokens: input + cached,
        priced_at: query.priced_at,
    })?;
    let cost = (input * prices.input + cached * prices.cached.unwrap_or(prices.input) + output * prices.output) / 1_000_000.0;
    Some(rounded_cost(cost))
}

/// GUI 턴은 result의 total_cost_usd, 터미널 턴은 상태줄 세션 누적 비용의 차이가 reportedCostUsd로 들어온다.
/// 서브에이전트·압축·내부 호출까지 든 공급자 합계라 토큰으로 다시 계산하지 않고, 없으면 비운다.
fn reported_claude_cost(usage: &Value) -> Option<f64> {
    nonnegative_number(usage.get("reportedCostUsd")).map(rounded_cost)
}

/// `value ?? 0`처럼 없는 값과 null을 같은 쪽으로 모은다.
fn defined(value: Option<&Value>) -> Option<&Value> {
    value.filter(|value| !value.is_null())
}

/// `nonnegativeNumber(value ?? 0)`. 숫자가 아닌 값이면 None이다.
fn counted_tokens(value: Option<&Value>) -> Option<f64> {
    match defined(value) {
        None => Some(0.0),
        value => nonnegative_number(value),
    }
}

fn nonnegative_number(value: Option<&Value>) -> Option<f64> {
    match value {
        Some(Value::Number(number)) => number.as_f64().filter(|number| number.is_finite() && *number >= 0.0),
        _ => None,
    }
}

/// JS `Number(left + right)`. 없는 쪽은 0이다. 한쪽이 문자열·배열·객체면 글자로 이어 붙인 뒤 숫자로 읽는다.
fn js_sum_number(left: Option<&Value>, right: Option<&Value>) -> f64 {
    let textual = |value: Option<&Value>| matches!(value, Some(Value::String(_) | Value::Array(_) | Value::Object(_)));
    if textual(left) || textual(right) {
        let text = |value: Option<&Value>| value.map_or_else(|| "0".to_string(), js_string);
        string_to_number(&(text(left) + &text(right)))
    } else {
        left.map_or(0.0, js_to_number) + right.map_or(0.0, js_to_number)
    }
}

/// pricing-catalog의 priceTimestamp. 글자는 Date.parse, 그 밖은 Number이고 잘못된 값은 단가 조회에서 지금으로 바뀐다.
fn price_timestamp(value: &Value) -> f64 {
    match value {
        Value::String(text) => date_parse(text).unwrap_or(f64::NAN),
        value => js_to_number(value),
    }
}

/// `Math.round(value * 1e8) / 1e8`. 비용은 0 이상이라 f64::round와 Math.round가 같다.
fn rounded_cost(value: f64) -> f64 {
    (value * 100_000_000.0).round() / 100_000_000.0
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn request_turn(requests: &[Value]) -> Value {
        let mut result = json!({ "requestUsages": requests });
        for field in TOKEN_FIELDS {
            let total: f64 = requests.iter().map(|request| request["usage"].get(field).and_then(Value::as_f64).unwrap_or(0.0)).sum();
            result[field] = json!(total);
        }
        result
    }

    fn at(text: &str) -> Option<f64> {
        date_parse(text)
    }

    fn codex<'a>(model: &'a str, fast_mode: bool, usage: &'a Value) -> CostQuery<'a> {
        CostQuery { backend: Some("codex"), model: Some(model), fast_mode, usage: Some(usage), priced_at: None }
    }

    fn antigravity<'a>(model: &'a str, usage: &'a Value, priced_at: Option<f64>) -> CostQuery<'a> {
        CostQuery { backend: Some("antigravity"), model: Some(model), fast_mode: false, usage: Some(usage), priced_at }
    }

    fn claude<'a>(model: Option<&'a str>, usage: &'a Value, priced_at: Option<f64>) -> CostQuery<'a> {
        CostQuery { backend: Some("claude"), model, fast_mode: false, usage: Some(usage), priced_at }
    }

    #[test]
    fn astra_turn_total_is_not_one_long_request() {
        let catalogs = PricingCatalogs::new();
        let request = json!({ "usage": { "inputTokens": 150_000, "outputTokens": 100 } });
        let usage = request_turn(&[request.clone(), request]);
        assert_eq!(estimate_token_cost(&catalogs, &codex("gpt-6-astra", false, &usage)), Some(6.015));
        assert_eq!(estimate_turn_token_cost(&catalogs, &codex("gpt-6-astra", false, &usage)), Some(3.01));
    }

    #[test]
    fn long_context_surcharge_starts_per_request_at_272001() {
        let catalogs = PricingCatalogs::new();
        let requests: Vec<Value> = [272_000, 272_001]
            .iter()
            .map(|input| json!({ "usage": { "inputTokens": input, "cachedInputTokens": 200_000, "outputTokens": 100 } }))
            .collect();
        assert_eq!(estimate_token_cost(&catalogs, &codex("gpt-6-astra", false, &requests[0]["usage"])), Some(0.925));
        assert_eq!(estimate_token_cost(&catalogs, &codex("gpt-6-astra", false, &requests[1]["usage"])), Some(1.84752));
        let usage = request_turn(&requests);
        assert_eq!(estimate_turn_token_cost(&catalogs, &codex("gpt-6-astra", false, &usage)), Some(2.77252));
    }

    #[test]
    fn request_model_and_fast_mode_apply_and_claude_keeps_reported_cost() {
        let catalogs = PricingCatalogs::new();
        let requests = [
            json!({ "model": "gpt-6-astra", "fastMode": true, "usage": { "inputTokens": 1000, "outputTokens": 100 } }),
            json!({ "model": "gpt-5.6-sol", "fastMode": false, "usage": { "inputTokens": 1000, "outputTokens": 100 } }),
        ];
        let expected: f64 = requests
            .iter()
            .map(|request| {
                let model = request["model"].as_str().unwrap();
                let fast = request["fastMode"].as_bool().unwrap();
                estimate_token_cost(&catalogs, &codex(model, fast, &request["usage"])).unwrap()
            })
            .fold(0.0, |sum, amount| sum + amount);
        let usage = request_turn(&requests);
        assert_eq!(estimate_turn_token_cost(&catalogs, &codex("gpt-6-astra", false, &usage)), Some(expected));
        let mut reported = usage.clone();
        reported["reportedCostUsd"] = json!(12.345678);
        assert_eq!(estimate_turn_token_cost(&catalogs, &claude(None, &reported, None)), Some(12.345678));
        assert_eq!(estimate_turn_token_cost(&catalogs, &claude(None, &usage, None)), None);
    }

    #[test]
    fn missing_or_mismatched_requests_do_not_guess_long_context() {
        let catalogs = PricingCatalogs::new();
        let usage = json!({ "inputTokens": 300_000, "outputTokens": 200 });
        assert_eq!(estimate_turn_token_cost(&catalogs, &codex("gpt-6-astra", false, &usage)), None);
        let mut partial = usage.clone();
        partial["requestUsages"] = json!([{ "usage": { "inputTokens": 150_000, "outputTokens": 100 } }]);
        assert_eq!(estimate_turn_token_cost(&catalogs, &codex("gpt-6-astra", false, &partial)), None);
        let mut missing = usage.clone();
        missing["requestUsages"] = json!([null]);
        assert_eq!(estimate_turn_token_cost(&catalogs, &codex("gpt-6-astra", false, &missing)), None);
        let short = json!({ "inputTokens": 150_000, "outputTokens": 100 });
        assert_eq!(estimate_turn_token_cost(&catalogs, &codex("gpt-6-astra", false, &short)), Some(1.505));
    }

    #[test]
    fn antigravity_prices_each_request_with_cached_length() {
        let catalogs = PricingCatalogs::new();
        let priced_at = at("2026-09-13T00:00:00Z");
        let request = json!({ "usage": { "inputTokens": 100_000, "cachedInputTokens": 50_000, "outputTokens": 100 } });
        let usage = request_turn(&[request.clone(), request.clone()]);
        for model in ["gemini-3.1-pro", "gemini-3.8-flash"] {
            let expected = estimate_token_cost(&catalogs, &antigravity(model, &request["usage"], priced_at)).unwrap() * 2.0;
            assert_eq!(estimate_turn_token_cost(&catalogs, &antigravity(model, &usage, priced_at)), Some(expected));
        }
    }

    fn cache_usage() -> Value {
        json!({ "inputTokens": 1_000, "cachedInputTokens": 200, "cacheWriteInputTokens": 100, "outputTokens": 50 })
    }

    #[test]
    fn sol_standard_and_fast_split_cache_kinds() {
        let catalogs = PricingCatalogs::new();
        let usage = cache_usage();
        assert_eq!(estimate_token_cost(&catalogs, &codex("gpt-5.6-sol", false, &usage)), Some(0.00438));
        assert_eq!(estimate_token_cost(&catalogs, &codex("gpt-5.6-sol", true, &usage)), Some(0.00876));
    }

    #[test]
    fn gemini_flash_prices_and_promotion_end() {
        let catalogs = PricingCatalogs::new();
        let usage = json!({ "inputTokens": 100_000, "cachedInputTokens": 20_000, "outputTokens": 5_000, "reasoningOutputTokens": 4_000 });
        let promotion = at("2026-08-28T00:00:00Z");
        assert_eq!(estimate_token_cost(&catalogs, &antigravity("gemini-3.7-flash", &usage, promotion)), Some(0.09525));
        let flash38 = estimate_token_cost(&catalogs, &antigravity("gemini-3.8-flash", &usage, promotion));
        assert_eq!(flash38, estimate_token_cost(&catalogs, &antigravity("gemini-3.7-flash", &usage, promotion)));
        assert_ne!(flash38, None);
        let regular = json!({ "inputTokens": 100_000, "cachedInputTokens": 20_000, "outputTokens": 5_000 });
        let after = at("2027-01-01T00:00:00Z");
        assert_eq!(estimate_token_cost(&catalogs, &antigravity("gemini-3.7-flash", &regular, after)), Some(0.1905));
    }

    #[test]
    fn gemini_pro_long_prompt_and_flash_output_rate() {
        let catalogs = PricingCatalogs::new();
        let pro = json!({ "inputTokens": 2_000, "cachedInputTokens": 200_000, "outputTokens": 1_000 });
        assert_eq!(estimate_token_cost(&catalogs, &antigravity("gemini-3.1-pro", &pro, None)), Some(0.106));
        let flash = json!({ "inputTokens": 1_000, "cachedInputTokens": 1_000, "outputTokens": 1_000 });
        assert_eq!(estimate_token_cost(&catalogs, &antigravity("gemini-3.5-flash", &flash, None)), Some(0.01065));
    }

    #[test]
    fn claude_keeps_cli_reported_cost_only() {
        let catalogs = PricingCatalogs::new();
        let reported = json!({ "reportedCostUsd": 0.13599125 });
        let fast = CostQuery { fast_mode: true, ..claude(Some("claude-opus-5"), &reported, None) };
        assert_eq!(estimate_token_cost(&catalogs, &fast), Some(0.13599125));
        let pipeline = json!({ "inputTokens": 2, "outputTokens": 4, "cachedInputTokens": 0, "cacheWriteInputTokens": 10, "reportedCostUsd": 0.42 });
        assert_eq!(estimate_token_cost(&catalogs, &claude(Some("claude-opus-5"), &pipeline, None)), Some(0.42));
        for (date, cost) in [("2026-08-19T00:00:00Z", 1.4559903), ("2026-09-01T00:00:00Z", 1.1559903)] {
            let usage = json!({ "reportedCostUsd": cost, "reportedSonnet5CostUsd": 1.1559903 });
            assert_eq!(estimate_token_cost(&catalogs, &claude(Some("claude-sonnet-5"), &usage, at(date))), Some(cost));
        }
        for (model, speed) in [("claude-opus-5", "standard"), ("claude-opus-5", "fast"), ("claude-sonnet-5", "standard"), ("opus[1m]", "standard")] {
            let usage = json!({
                "inputTokens": 10, "outputTokens": 5, "cachedInputTokens": 100, "cacheWriteInputTokens": 50,
                "cacheWrite5mInputTokens": 20, "cacheWrite1hInputTokens": 30, "speed": speed, "inferenceGeo": "global",
                "reportedCostUsd": null,
            });
            let query = CostQuery { fast_mode: speed == "fast", ..claude(Some(model), &usage, None) };
            assert_eq!(estimate_token_cost(&catalogs, &query), None);
        }
        let zero = json!({ "inputTokens": 10, "outputTokens": 5, "reportedCostUsd": 0 });
        assert_eq!(estimate_token_cost(&catalogs, &claude(Some("opus[1m]"), &zero, None)), Some(0.0));
    }

    #[test]
    fn unknown_models_get_no_cost() {
        let catalogs = PricingCatalogs::new();
        let usage = cache_usage();
        assert_eq!(estimate_token_cost(&catalogs, &codex("unknown", false, &usage)), None);
        assert_eq!(estimate_token_cost(&catalogs, &antigravity("unknown", &usage, None)), None);
    }

    #[test]
    fn js_coercions_match_node() {
        assert_eq!(js_sum_number(Some(&json!(5)), Some(&json!("3"))), 53.0);
        assert_eq!(js_sum_number(Some(&json!(5)), Some(&json!("e3"))), 5_000.0);
        assert_eq!(js_sum_number(Some(&json!(5)), Some(&json!([]))), 5.0);
        assert_eq!(js_sum_number(Some(&json!(5)), Some(&json!(true))), 6.0);
        assert!(js_sum_number(Some(&json!(5)), Some(&json!({}))).is_nan());
        assert_eq!(js_sum_number(Some(&json!("5")), None), 50.0);
        assert_eq!(price_timestamp(&json!("2026-09-13T00:00:00Z")), 1_789_257_600_000.0);
        assert_eq!(price_timestamp(&json!(true)), 1.0);
        assert!(price_timestamp(&json!("not a date")).is_nan());
        assert_eq!(counted_tokens(Some(&json!(null))), Some(0.0));
        assert_eq!(counted_tokens(Some(&json!("1"))), None);
        assert_eq!(counted_tokens(Some(&json!(-1))), None);
    }
}
