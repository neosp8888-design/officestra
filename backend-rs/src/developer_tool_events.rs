// 개발 도구 훅이 보내는 진행 표시 이벤트를 검사한다. Node developer-tool-events.mjs와 같다.

use std::collections::{BTreeMap, HashSet};
use std::sync::LazyLock;

use regex::Regex;
use serde_json::{Value, json};

use crate::js::{js_length, js_trim, number_to_string, string_to_number};

/// 한 턴에 받을 수 있는 서로 다른 이벤트 열쇠 수. 이미 받은 열쇠의 완료는 한도와 관계없이 받는다.
pub const DEVELOPER_TOOL_EVENT_LIMIT: usize = 40;

static EVENT_KEY: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^developer-tool:[a-z0-9:-]{1,130}$").unwrap());

/// developerToolEnvironment. 포트 값이 숫자가 아니면 Node Number()처럼 NaN이 주소에 들어간다.
pub fn developer_tool_environment(character_id: &str, base: &BTreeMap<String, String>, mode: &str) -> BTreeMap<String, String> {
    let port = base.get("OFFICE_BACKEND_PORT").map_or(4317.0, |value| string_to_number(value));
    BTreeMap::from([
        ("OFFICESTRA_TOOL_CHARACTER_ID".to_string(), character_id.to_string()),
        ("OFFICESTRA_TOOL_MODE".to_string(), mode.to_string()),
        (
            "OFFICESTRA_TOOL_EVENTS_URL".to_string(),
            format!("http://127.0.0.1:{}/api/developer-tool-events", number_to_string(port)),
        ),
    ])
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeveloperToolEvent {
    pub character_id: String,
    pub session_id: String,
    pub mode: String,
    pub event_key: String,
    pub status: String,
    pub text: String,
}

impl DeveloperToolEvent {
    /// 활동 기록에 넘길 값. 종류는 언제나 tool이라 훅이 다른 종류나 권한 값을 넣을 수 없다.
    pub fn activity(&self) -> Value {
        json!({ "kind": "tool", "eventKey": self.event_key, "status": self.status, "text": self.text })
    }
}

/// parseDeveloperToolEvent. 조건을 하나라도 어기면 None이다. 길이는 UTF-16 단위로 잰다.
pub fn parse_developer_tool_event(body: &Value) -> Option<DeveloperToolEvent> {
    let text_field = |value: Option<&Value>, maximum: usize| {
        value.and_then(Value::as_str).filter(|text| !text.is_empty() && js_length(text) <= maximum).map(str::to_string)
    };
    let character_id = text_field(body.get("characterId"), 100)?;
    let session_id = text_field(body.get("sessionId"), 200)?;
    let mode = body.get("mode").and_then(Value::as_str).filter(|mode| matches!(*mode, "gui" | "terminal"))?;
    let activity = body.get("activity");
    let event_key = match activity.and_then(|activity| activity.get("eventKey")) {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(key)) => key.clone(),
        // Node RegExp.test는 문자열이 아닌 값을 String()으로 바꾼다. 숫자·객체는 접두사가 맞을 수 없다.
        Some(_) => return None,
    };
    if !EVENT_KEY.is_match(&event_key) {
        return None;
    }
    let status = activity
        .and_then(|activity| activity.get("status"))
        .and_then(Value::as_str)
        .filter(|status| matches!(*status, "running" | "completed"))?;
    let raw = activity.and_then(|activity| activity.get("text")).and_then(Value::as_str)?;
    if js_trim(raw).is_empty() || js_length(raw) > 300 {
        return None;
    }
    let replaced: String = raw.chars().map(|ch| if (ch as u32) < 0x20 || ch == '\u{7f}' { ' ' } else { ch }).collect();
    Some(DeveloperToolEvent {
        character_id,
        session_id,
        mode: mode.to_string(),
        event_key,
        status: status.to_string(),
        text: js_trim(&replaced).to_string(),
    })
}

/// 턴마다 받은 이벤트 열쇠. Node state.developerToolEventKeys.
#[derive(Debug, Default)]
pub struct DeveloperToolEventKeys(HashSet<String>);

impl DeveloperToolEventKeys {
    /// 새 열쇠가 한도를 넘으면 false. 받은 열쇠는 기억한다.
    pub fn admit(&mut self, key: &str) -> bool {
        if !self.0.contains(key) && self.0.len() >= DEVELOPER_TOOL_EVENT_LIMIT {
            return false;
        }
        self.0.insert(key.to_string());
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event() -> Value {
        json!({
            "characterId": "test", "sessionId": "cli-session", "mode": "gui",
            "activity": { "eventKey": "developer-tool:graft:abc", "status": "running", "text": "Graft · 관련 코드 탐색 중" },
        })
    }

    fn with(mut body: Value, path: &[&str], value: Value) -> Value {
        let mut target = &mut body;
        for key in &path[..path.len() - 1] {
            target = &mut target[*key];
        }
        target[path[path.len() - 1]] = value;
        body
    }

    #[test]
    fn 표시_이벤트는_종류·권한·무제한_내용을_넣을_수_없다() {
        let forged = with(with(event(), &["activity", "kind"], json!("command")), &["activity", "permissionDecision"], json!("allow"));
        assert_eq!(parse_developer_tool_event(&forged).unwrap().activity()["kind"], "tool");
        for body in [
            Value::Null,
            json!({}),
            with(event(), &["mode"], json!("unknown")),
            with(event(), &["sessionId"], json!("")),
            with(event(), &["activity", "eventKey"], json!("other:key")),
            with(event(), &["activity", "text"], json!("x".repeat(301))),
        ] {
            assert_eq!(parse_developer_tool_event(&body), None, "{body}");
        }
    }

    #[test]
    fn 제어_문자는_공백으로_바꾸고_양끝을_다듬는다() {
        let parsed = parse_developer_tool_event(&with(event(), &["activity", "text"], json!("\u{1}  탐색\n중\u{7f}"))).unwrap();
        assert_eq!(parsed.text, "탐색 중");
        assert_eq!(parse_developer_tool_event(&with(event(), &["activity", "text"], json!("\u{3000}"))), None);
        // UTF-16 단위로 300까지 받는다. 이모지 하나는 2단위다.
        assert!(parse_developer_tool_event(&with(event(), &["activity", "text"], json!("😀".repeat(150)))).is_some());
        assert_eq!(parse_developer_tool_event(&with(event(), &["activity", "text"], json!("😀".repeat(151)))), None);
        assert_eq!(parse_developer_tool_event(&with(event(), &["activity", "eventKey"], json!("developer-tool:A"))), None);
        assert_eq!(parse_developer_tool_event(&with(event(), &["activity", "eventKey"], json!("developer-tool:a\n"))), None);
    }

    #[test]
    fn 환경은_모드와_포트를_담는다() {
        let base = BTreeMap::from([("OFFICE_BACKEND_PORT".to_string(), "4320".to_string())]);
        let environment = developer_tool_environment("test", &base, "terminal");
        assert_eq!(environment["OFFICESTRA_TOOL_MODE"], "terminal");
        assert_eq!(environment["OFFICESTRA_TOOL_EVENTS_URL"], "http://127.0.0.1:4320/api/developer-tool-events");
        let default = developer_tool_environment("test", &BTreeMap::new(), "gui");
        assert!(default["OFFICESTRA_TOOL_EVENTS_URL"].ends_with(":4317/api/developer-tool-events"));
        let invalid = BTreeMap::from([("OFFICE_BACKEND_PORT".to_string(), "abc".to_string())]);
        assert!(developer_tool_environment("test", &invalid, "gui")["OFFICESTRA_TOOL_EVENTS_URL"].contains(":NaN/"));
    }

    #[test]
    fn 한도는_새_열쇠만_막고_받은_열쇠의_완료는_받는다() {
        let mut keys = DeveloperToolEventKeys::default();
        for index in 0..40 {
            assert!(keys.admit(&format!("developer-tool:graft:{index}")));
        }
        assert!(!keys.admit("developer-tool:graft:abc"));
        assert!(keys.admit("developer-tool:graft:0"));
    }
}
