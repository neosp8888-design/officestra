// 턴 사용량 차이·누적·복구. Node agent-runtime.mjs의 codexUsageDelta·accumulatedUsage·latestUsageFromTail 묶음과 같다.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};

use serde_json::{Map, Value};

use crate::agent_event_parser::{USAGE_COUNT_FIELDS, claude_session_usage};
use crate::js::{js_to_number, parse_js_json};

const ROLLOUT_TAIL_CHUNK_BYTES: u64 = 64 * 1024;

/// 누적 합산하는 숫자 칸. 토큰 일곱 칸 뒤에 보고 비용 두 칸이 붙는다.
const ACCUMULATED_FIELDS: [&str; 9] = [
    "inputTokens",
    "outputTokens",
    "cachedInputTokens",
    "cacheWriteInputTokens",
    "cacheWrite5mInputTokens",
    "cacheWrite1hInputTokens",
    "reasoningOutputTokens",
    "reportedCostUsd",
    "reportedSonnet5CostUsd",
];

fn number(value: Option<&Value>) -> Option<f64> {
    value.and_then(Value::as_f64)
}

/// emptyUsage.
pub fn empty_usage() -> Value {
    let mut usage = Map::new();
    for field in USAGE_COUNT_FIELDS {
        usage.insert(field.into(), Value::Null);
    }
    for field in ["serviceTier", "speed", "inferenceGeo", "reportedCostUsd"] {
        usage.insert(field.into(), Value::Null);
    }
    Value::Object(usage)
}

/// codexUsageDelta. 누적 세션의 현재 값에서 시작 값을 뺀다. 어느 칸이든 줄었으면 같은 세션이 아니라 None이다.
pub fn codex_usage_delta(usage: Option<&Value>, baseline: Option<&Value>) -> Option<Value> {
    let (usage, baseline) = (usage.filter(|value| !value.is_null())?, baseline.filter(|value| !value.is_null())?);
    let mut result = usage.as_object().cloned().unwrap_or_default();
    for field in USAGE_COUNT_FIELDS {
        let current = usage.get(field).filter(|value| !value.is_null());
        let previous = baseline.get(field).filter(|value| !value.is_null());
        let value = match (current, previous) {
            (None, _) => Value::Null,
            (Some(current), None) => current.clone(),
            (Some(current), Some(previous)) => {
                let (current, previous) = (js_to_number(current), js_to_number(previous));
                if current < previous {
                    return None;
                }
                // 숫자가 아니면 Node 뺄셈처럼 NaN이 되고 JSON에서는 null이다.
                Value::from(current - previous)
            }
        };
        result.insert(field.into(), value);
    }
    Some(Value::Object(result))
}

/// accumulatedUsage. 둘 다 없으면 null, 하나라도 있으면 없는 쪽을 0으로 더한다. 글자 칸은 새 값이 우선이다.
pub fn accumulated_usage(current: Option<&Value>, delta: Option<&Value>) -> Option<Value> {
    let current = current.filter(|value| !value.is_null());
    let delta = delta.filter(|value| !value.is_null());
    let (current, delta) = match (current, delta) {
        (None, delta) => return delta.cloned(),
        (Some(current), None) => return Some(current.clone()),
        (Some(current), Some(delta)) => (current, delta),
    };
    let mut result = current.as_object().cloned().unwrap_or_default();
    for field in ACCUMULATED_FIELDS {
        let (left, right) = (current.get(field).filter(|v| !v.is_null()), delta.get(field).filter(|v| !v.is_null()));
        let value = if left.is_none() && right.is_none() {
            Value::Null
        } else {
            Value::from(number(left).unwrap_or(0.0) + number(right).unwrap_or(0.0))
        };
        result.insert(field.into(), value);
    }
    for field in ["serviceTier", "speed", "inferenceGeo"] {
        let value = [delta.get(field), current.get(field)].into_iter().flatten().find(|value| !value.is_null());
        result.insert(field.into(), value.cloned().unwrap_or(Value::Null));
    }
    Some(Value::Object(result))
}

/// usageForTurn. 재개한 Codex·Antigravity 세션은 누적값이라 시작 값을 빼야 하고, 시작 값이 없으면 버린다.
pub fn usage_for_turn(resumed_cumulative_session: bool, baseline: Option<&Value>, usage: Value) -> Option<Value> {
    if !resumed_cumulative_session {
        return Some(usage);
    }
    let baseline = baseline.filter(|value| !value.is_null())?;
    Some(codex_usage_delta(Some(&usage), Some(baseline)).unwrap_or(usage))
}

fn nonnegative_token_count(value: Option<&Value>) -> Value {
    match value.and_then(Value::as_f64) {
        Some(count) if count.is_finite() && count >= 0.0 => value.cloned().unwrap_or(Value::Null),
        _ => Value::Null,
    }
}

/// codexRolloutUsage. token_count 기록의 total_token_usage만 읽고 입력·출력이 모두 있어야 한다.
pub fn codex_rollout_usage(line: &str) -> Option<Value> {
    let record = parse_js_json(line).ok()?;
    let payload = record.get("payload")?;
    if payload.get("type").and_then(Value::as_str) != Some("token_count") {
        return None;
    }
    let usage = payload.get("info").and_then(|info| info.get("total_token_usage"));
    let field = |name: &str| nonnegative_token_count(usage.and_then(|usage| usage.get(name)));
    let (input, output) = (field("input_tokens"), field("output_tokens"));
    if input.is_null() || output.is_null() {
        return None;
    }
    let mut result = Map::new();
    result.insert("inputTokens".into(), input);
    result.insert("outputTokens".into(), output);
    result.insert("cachedInputTokens".into(), field("cached_input_tokens"));
    result.insert("cacheWriteInputTokens".into(), field("cache_write_input_tokens"));
    result.insert("cacheWrite5mInputTokens".into(), Value::Null);
    result.insert("cacheWrite1hInputTokens".into(), Value::Null);
    result.insert("reasoningOutputTokens".into(), field("reasoning_output_tokens"));
    for name in ["serviceTier", "speed", "inferenceGeo", "reportedCostUsd"] {
        result.insert(name.into(), Value::Null);
    }
    Some(Value::Object(result))
}

/// latestCodexUsageFromRollout.
pub fn latest_codex_usage_from_rollout(path: Option<&str>) -> Option<Value> {
    latest_usage_from_tail(path, codex_rollout_usage)
}

/// latestClaudeUsageFromSession. 중단된 Claude 턴은 결과 이벤트가 없어 세션 기록 끝에서 사용량을 읽는다.
pub fn latest_claude_usage_from_session(path: Option<&str>) -> Option<Value> {
    latest_usage_from_tail(path, claude_session_usage)
}

/// latestUsageFromTail. 파일 끝부터 64KiB씩 거꾸로 읽으며 마지막 줄부터 사용량을 찾는다.
/// 조각 첫 줄은 앞 조각과 이어 붙일 때까지 미룬다. 읽기 오류는 null이다.
pub fn latest_usage_from_tail(path: Option<&str>, extract: impl Fn(&str) -> Option<Value>) -> Option<Value> {
    let path = path.filter(|path| !path.is_empty())?;
    let mut file = File::open(path).ok()?;
    let mut end = file.metadata().ok()?.len();
    // 줄은 바이트로 이어 붙인다. Node는 조각마다 UTF-8로 풀어 경계의 글자가 깨질 수 있으나 사용량 숫자에는 영향이 없다.
    let mut leading: Vec<u8> = Vec::new();
    while end > 0 {
        let start = end.saturating_sub(ROLLOUT_TAIL_CHUNK_BYTES);
        let mut buffer = vec![0_u8; (end - start) as usize];
        file.seek(SeekFrom::Start(start)).ok()?;
        let read = read_up_to(&mut file, &mut buffer).ok()?;
        buffer.truncate(read);
        buffer.extend_from_slice(&leading);
        let mut lines: Vec<&[u8]> = buffer.split(|byte| *byte == b'\n').collect();
        let first = lines.remove(0).to_vec();
        for line in lines.iter().rev() {
            if let Some(usage) = extract(&String::from_utf8_lossy(line)) {
                return Some(usage);
            }
        }
        leading = first;
        end = start;
    }
    extract(&String::from_utf8_lossy(&leading))
}

/// readSync 한 번처럼 버퍼를 채울 때까지 읽는다. 파일이 줄었으면 읽은 만큼만 돌려준다.
fn read_up_to(file: &mut File, buffer: &mut [u8]) -> std::io::Result<usize> {
    let mut total = 0;
    while total < buffer.len() {
        let count = file.read(&mut buffer[total..])?;
        if count == 0 {
            break;
        }
        total += count;
    }
    Ok(total)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn antigravity_단계별_사용량은_결과_누적값과_중복하지_않고_합산한다() {
        let first = json!({ "inputTokens": 100, "outputTokens": 10, "cachedInputTokens": 80, "reasoningOutputTokens": 3 });
        let second = json!({ "inputTokens": 40, "outputTokens": 5, "cachedInputTokens": 20, "reasoningOutputTokens": 2 });
        let total = accumulated_usage(Some(&first), Some(&second)).unwrap();
        let numbers: Vec<Option<f64>> = ["inputTokens", "outputTokens", "cachedInputTokens", "reasoningOutputTokens"]
            .iter()
            .map(|field| total[*field].as_f64())
            .collect();
        assert_eq!(numbers, [Some(140.0), Some(15.0), Some(100.0), Some(5.0)]);
        for field in [
            "cacheWriteInputTokens",
            "cacheWrite5mInputTokens",
            "cacheWrite1hInputTokens",
            "reportedCostUsd",
            "reportedSonnet5CostUsd",
            "serviceTier",
            "speed",
            "inferenceGeo",
        ] {
            assert_eq!(total[field], Value::Null, "{field}");
        }
        assert_eq!(total.as_object().unwrap().len(), 12);
        assert_eq!(accumulated_usage(None, Some(&first)), Some(first.clone()));
        assert_eq!(accumulated_usage(Some(&first), None), Some(first.clone()));
        assert_eq!(accumulated_usage(None, None), None);
        let tiers = accumulated_usage(Some(&json!({ "serviceTier": "a", "speed": "s" })), Some(&json!({ "serviceTier": "b" }))).unwrap();
        assert_eq!((tiers["serviceTier"].clone(), tiers["speed"].clone()), (json!("b"), json!("s")));
    }

    #[test]
    fn 누적_세션_차이는_줄어든_칸이_있으면_버린다() {
        let baseline = json!({ "inputTokens": 100, "outputTokens": 10, "cachedInputTokens": null, "reasoningOutputTokens": 2 });
        let current = json!({ "inputTokens": 150, "outputTokens": 12, "cachedInputTokens": 30, "reasoningOutputTokens": null, "serviceTier": "x" });
        let delta = codex_usage_delta(Some(&current), Some(&baseline)).unwrap();
        assert_eq!(delta["inputTokens"].as_f64(), Some(50.0));
        assert_eq!(delta["outputTokens"].as_f64(), Some(2.0));
        assert_eq!(delta["cachedInputTokens"], json!(30), "시작 값이 없으면 현재 값 그대로");
        assert_eq!(delta["reasoningOutputTokens"], Value::Null);
        assert_eq!(delta["serviceTier"], "x");
        assert_eq!(delta["cacheWriteInputTokens"], Value::Null, "없는 칸도 null로 채운다");
        let shrunk = json!({ "inputTokens": 90, "outputTokens": 12 });
        assert_eq!(codex_usage_delta(Some(&shrunk), Some(&baseline)), None);
        assert_eq!(codex_usage_delta(None, Some(&baseline)), None);

        assert_eq!(usage_for_turn(false, None, current.clone()), Some(current.clone()));
        assert_eq!(usage_for_turn(true, None, current.clone()), None, "재개 세션에 시작 값이 없으면 버린다");
        assert_eq!(usage_for_turn(true, Some(&baseline), shrunk.clone()), Some(shrunk), "차이가 없으면 받은 값");
    }

    #[test]
    fn 롤아웃_끝에서_마지막_token_count를_찾는다() {
        let directory = std::env::temp_dir().join(format!("usage-tail-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join("rollout.jsonl");
        let record = |input: u64| {
            json!({ "payload": { "type": "token_count", "info": { "total_token_usage": {
                "input_tokens": input, "output_tokens": 7, "cached_input_tokens": 3, "reasoning_output_tokens": 1 } } } })
            .to_string()
        };
        let filler = "x".repeat(70_000);
        let body = format!("{}\n{}\n{{\"payload\":{{\"type\":\"other\"}}}}\n{filler}\n", record(10), record(20));
        std::fs::write(&path, body).unwrap();
        let usage = latest_codex_usage_from_rollout(path.to_str()).unwrap();
        assert_eq!(usage["inputTokens"], json!(20));
        assert_eq!(usage["cacheWrite5mInputTokens"], Value::Null);
        assert_eq!(usage.as_object().unwrap().keys().next().map(String::as_str), Some("inputTokens"));

        std::fs::write(&path, record(5)).unwrap();
        assert_eq!(latest_codex_usage_from_rollout(path.to_str()).unwrap()["inputTokens"], json!(5), "줄바꿈 없는 한 줄");
        assert_eq!(latest_codex_usage_from_rollout(Some("/none/rollout.jsonl")), None);
        assert_eq!(latest_codex_usage_from_rollout(None), None);
        let negative = json!({ "payload": { "type": "token_count", "info": { "total_token_usage": { "input_tokens": -1, "output_tokens": 1 } } } });
        assert_eq!(codex_rollout_usage(&negative.to_string()), None);
        std::fs::remove_dir_all(&directory).unwrap();
    }
}
