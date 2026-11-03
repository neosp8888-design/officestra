// Claude 세션 기록 끝에서부터 마지막 cost-state 줄을 찾아 이어받을 누적 비용을 읽는다. Node claude-cost-state.mjs를 옮겼다.

use std::fs::File;
use std::os::unix::fs::FileExt;

use serde_json::{Value, json};

const CHUNK_BYTES: u64 = 64 * 1024;

/// Claude가 재개할 때 이어 쓰는 세션 누적값. 토큰 단가로 다시 만들지 않는다.
#[derive(Clone, Debug, PartialEq)]
pub struct ClaudeCostState {
    pub total_cost_usd: f64,
    pub model_usage: Value,
}

impl ClaudeCostState {
    pub fn to_json(&self) -> Value {
        json!({ "totalCostUsd": self.total_cost_usd, "modelUsage": self.model_usage })
    }
}

/// readClaudeCostState. 파일을 64KiB씩 뒤에서부터 읽어 그 세션의 마지막 올바른 cost-state를 돌려준다.
pub fn read_claude_cost_state(path: Option<&str>, session_id: Option<&str>) -> Option<ClaudeCostState> {
    let path = path.filter(|path| !path.is_empty())?;
    let session_id = session_id.filter(|id| !id.is_empty())?;
    let file = File::open(path).ok()?;
    let mut offset = file.metadata().ok()?.len();
    let mut remainder = Vec::new();
    while offset > 0 {
        let size = offset.min(CHUNK_BYTES);
        offset -= size;
        let mut data = vec![0; size as usize];
        let read = file.read_at(&mut data, offset).ok()?;
        data.truncate(read);
        data.extend_from_slice(&remainder);
        let mut end = data.len();
        for index in (0..data.len()).rev() {
            if data[index] != b'\n' {
                continue;
            }
            if let Some(state) = parse_state(&data[index + 1..end], session_id) {
                return Some(state);
            }
            end = index;
        }
        data.truncate(end);
        remainder = data;
    }
    parse_state(&remainder, session_id)
}

fn parse_state(bytes: &[u8], session_id: &str) -> Option<ClaudeCostState> {
    let value: Value = serde_json::from_str(&String::from_utf8_lossy(bytes)).ok()?;
    if value.get("type")? != "cost-state" || value.get("sessionId")?.as_str()? != session_id {
        return None;
    }
    let total = value.get("totalCostUSD")?.as_f64().filter(|total| total.is_finite() && *total >= 0.0)?;
    let model_usage = value.get("modelUsage").filter(|usage| usage.is_object())?;
    Some(ClaudeCostState { total_cost_usd: total, model_usage: model_usage.clone() })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resume_reads_latest_matching_state_across_chunks_and_bad_tail() {
        let directory = std::env::temp_dir().join(format!("claude-cost-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join("session.jsonl");
        let state = |cost: Value, id: &str| {
            json!({ "type": "cost-state", "sessionId": id, "totalCostUSD": cost, "modelUsage": { "opus": { "costUSD": cost, "outputTokens": 100 } } })
        };
        let lines = [
            state(json!(10), "session"),
            json!({ "type": "user", "text": "한글".repeat(40_000) }),
            state(json!(39.985636), "session"),
            json!({ "type": "assistant", "text": "긴 내용".repeat(40_000) }),
            state(json!(99), "other"),
            state(Value::Null, "session"),
        ];
        let body = lines.iter().map(Value::to_string).collect::<Vec<_>>().join("\n") + "\n{\"unfinished\":";
        std::fs::write(&path, body).unwrap();
        let text = path.to_str().unwrap();
        let found = read_claude_cost_state(Some(text), Some("session")).unwrap();
        assert_eq!(
            found.to_json(),
            json!({ "totalCostUsd": 39.985636, "modelUsage": { "opus": { "costUSD": 39.985636, "outputTokens": 100 } } })
        );
        assert_eq!(read_claude_cost_state(Some(text), Some("missing")), None);
        let missing = directory.join("missing");
        assert_eq!(read_claude_cost_state(missing.to_str(), Some("session")), None);
        std::fs::write(&path, state(json!(0), "session").to_string()).unwrap();
        assert_eq!(read_claude_cost_state(Some(text), Some("session")).unwrap().total_cost_usd, 0.0);
        assert_eq!(read_claude_cost_state(Some(""), Some("session")), None);
        assert_eq!(read_claude_cost_state(Some(text), Some("")), None);
        std::fs::remove_dir_all(&directory).unwrap();
    }
}
