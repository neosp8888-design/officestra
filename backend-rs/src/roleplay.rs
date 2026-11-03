// 직원별 롤플레이 대화. 리스 프리셋·모듈 해독, CBS, 프롬프트 조립, 저장·모델 호출 서비스를 묶는다(Node roleplay-*.mjs 이식).

pub mod cbs;
pub mod js_regex;
pub mod js_sort;
pub mod prompt;
pub mod risu_format;
pub mod service;

pub use service::{RoleplayError, RoleplayService};

use serde_json::Value;

use crate::js::is_js_integer;

/// 가져온 리스 프리셋 또는 모듈. data는 해독한 JSON 그대로다.
#[derive(Debug, Clone, PartialEq)]
pub struct Asset {
    pub id: String,
    pub kind: String,
    pub name: String,
    pub data: Value,
    pub imported_at: String,
}

/// JSON.stringify의 숫자. 유한하지 않으면 null이고 정수는 정수로 둔다.
pub(crate) fn js_number(number: f64) -> Value {
    if !number.is_finite() {
        return Value::Null;
    }
    if is_js_integer(number) && number.abs() < 9_007_199_254_740_992.0 {
        return Value::from(number as i64);
    }
    serde_json::Number::from_f64(number).map_or(Value::Null, Value::Number)
}

/// 저장된 대화 메시지. role은 user 또는 char다.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ChatMessage {
    pub id: String,
    pub role: String,
    pub content: String,
    /// ISO 시각. CBS idle_duration이 읽는다. 없으면 리스의 옛 메시지처럼 다룬다.
    pub created_at: Option<String>,
}
