// Antigravity 대화 SQLite에 남은 단계별 추론 텍스트를 읽기 전용으로 해석한다. Node의 antigravity-reasoning.mjs를 옮겼다.

use std::path::Path;

use serde::Serialize;

use crate::antigravity_local_state::{
    SqliteValue, antigravity_conversation_path, nested_fields, open_conversation, protobuf_fields, sqlite_value,
    utf8_field,
};
use crate::js::{is_js_integer, js_length, utf16_prefix};

// agy는 응답 단계를 step_type 15로 남기고, 그 payload의 20번 필드 안 3번 필드에만 추론 요약을 담는다.
// 1번과 8번 필드는 사용자에게 보이는 답변이다. node:sqlite는 JS 숫자를 REAL로 묶으므로 묶는 값도 실수로 둔다.
const AGENT_RESPONSE_STEP_TYPE: f64 = 15.0;
const RESPONSE_FIELD: u128 = 20;
const REASONING_FIELD: u128 = 3;
const MAX_PAYLOAD_BYTES: f64 = 256.0 * 1024.0;
// 다른 백엔드의 추론 활동과 같은 상한을 쓴다.
const MAX_REASONING_LENGTH: usize = 6_000;
// agy는 단계가 끝나야 status를 3으로 바꾼다. 그 전까지는 추론이 자라는 중이다.
const STEP_STATUS_DONE: f64 = 3.0;

/// 단계 하나의 추론. step_index는 `Number(row.idx)`라 NaN이면 JSON에서 null이 된다.
#[derive(Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StepReasoning {
    pub step_index: f64,
    pub text: String,
    pub done: bool,
}

/// 대화 SQLite의 마지막 단계 번호. 턴 시작 전 시점의 기준선으로 쓴다.
pub fn antigravity_latest_step_index(session_id: &str, root: &str) -> Option<f64> {
    let path = antigravity_conversation_path(session_id, root)?;
    if !Path::new(&path).exists() {
        return None;
    }
    let database = open_conversation(&path).ok()?;
    match database.query_row("SELECT MAX(idx) AS max_idx FROM steps", [], |row| sqlite_value(row, 0)).ok()? {
        SqliteValue::Number(index) if is_js_integer(index) => Some(index),
        _ => None,
    }
}

/// 추론은 단계가 끝날 때가 아니라 진행 중에도 쌓이므로, 기준선 이후 단계별 추론을 돌려주고 갱신 여부는 호출자가 판단한다.
/// min_step_index는 정수일 때만 쓴다.
pub fn antigravity_step_reasonings(session_id: &str, min_step_index: Option<f64>, root: &str) -> Vec<StepReasoning> {
    let Some(path) = antigravity_conversation_path(session_id, root) else {
        return Vec::new();
    };
    if !Path::new(&path).exists() {
        return Vec::new();
    }
    // 대화 파일이 갱신되는 도중의 읽기 실패는 다음 폴링에서 회복한다.
    step_reasonings(&path, min_step_index.filter(|index| is_js_integer(*index))).unwrap_or_default()
}

fn step_reasonings(path: &str, min_step_index: Option<f64>) -> rusqlite::Result<Vec<StepReasoning>> {
    let database = open_conversation(path)?;
    let minimum = if min_step_index.is_some() { "AND idx >= ?" } else { "" };
    let mut statement = database.prepare(&format!(
        "SELECT idx, status, step_payload FROM steps WHERE step_type = ? AND step_payload IS NOT NULL AND length(step_payload) <= ? {minimum} ORDER BY idx"
    ))?;
    let parameters = [AGENT_RESPONSE_STEP_TYPE, MAX_PAYLOAD_BYTES].into_iter().chain(min_step_index);
    let rows = statement.query_map(rusqlite::params_from_iter(parameters), |row| {
        Ok((sqlite_value(row, 0)?, sqlite_value(row, 1)?, sqlite_value(row, 2)?))
    })?;
    let mut reasonings = Vec::new();
    for row in rows {
        let (index, status, payload) = row?;
        let Some(text) = payload.buffer().and_then(|payload| antigravity_step_payload_reasoning(&payload)) else {
            continue;
        };
        reasonings.push(StepReasoning { step_index: index.number(), text, done: status.number() == STEP_STATUS_DONE });
    }
    Ok(reasonings)
}

/// 응답 단계 payload에서 답변이 아닌 추론 필드만 읽는다. 비었거나 깨졌으면 활동을 만들지 않도록 None이다.
pub fn antigravity_step_payload_reasoning(value: &[u8]) -> Option<String> {
    let response = nested_fields(&protobuf_fields(value).ok()?, RESPONSE_FIELD).ok()?;
    let text = utf8_field(&response, REASONING_FIELD);
    if text.is_empty() {
        return None;
    }
    if js_length(&text) <= MAX_REASONING_LENGTH {
        return Some(text);
    }
    Some(format!("{}…", utf16_prefix(&text, MAX_REASONING_LENGTH - 1)))
}

#[cfg(test)]
mod tests {
    use std::fs;

    use rusqlite::Connection;

    use super::*;
    use crate::js::{join_path, to_js_json};

    const SESSION_ID: &str = "9dc0e8e9-427e-43a8-9803-345c9cda9d2c";

    fn varint(mut value: usize) -> Vec<u8> {
        let mut bytes = Vec::new();
        loop {
            let mut byte = (value & 0x7f) as u8;
            value >>= 7;
            if value > 0 {
                byte |= 0x80;
            }
            bytes.push(byte);
            if value == 0 {
                return bytes;
            }
        }
    }

    fn bytes_field(number: usize, value: &[u8]) -> Vec<u8> {
        [varint((number << 3) | 2), varint(value.len()), value.to_vec()].concat()
    }

    /// agy 응답 단계는 20번 필드 안에 답변(1·8번)과 추론(3번)을 함께 담는다.
    fn step_payload(answer: Option<&str>, reasoning: Option<&str>) -> Vec<u8> {
        let mut response = Vec::new();
        if let Some(answer) = answer {
            response.extend(bytes_field(1, answer.as_bytes()));
        }
        if let Some(reasoning) = reasoning {
            response.extend(bytes_field(3, reasoning.as_bytes()));
        }
        if let Some(answer) = answer {
            response.extend(bytes_field(8, answer.as_bytes()));
        }
        [bytes_field(5, &bytes_field(12, b"execution-1")), bytes_field(20, &response)].concat()
    }

    fn temporary_root(name: &str) -> String {
        let root = std::env::temp_dir().join(format!("officestra-agy-reasoning-{name}-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        root.to_string_lossy().into_owned()
    }

    fn reasoning(step_index: f64, text: &str, done: bool) -> StepReasoning {
        StepReasoning { step_index, text: text.into(), done }
    }

    const STEPS_TABLE: &str = "CREATE TABLE steps (idx INTEGER PRIMARY KEY, step_type INTEGER, status INTEGER, step_payload BLOB)";

    #[test]
    fn 응답_단계_payload에서_답변이_아니라_추론_필드만_읽는다() {
        assert_eq!(
            antigravity_step_payload_reasoning(&step_payload(
                Some("요청하신 파일을 만들었습니다."),
                Some("**Prioritizing Tool Usage**\n\n도구 선택을 먼저 검토했다.")
            ))
            .as_deref(),
            Some("**Prioritizing Tool Usage**\n\n도구 선택을 먼저 검토했다.")
        );
    }

    #[test]
    fn 추론이_비어_있는_단계는_활동을_만들지_않도록_none을_준다() {
        assert_eq!(antigravity_step_payload_reasoning(&step_payload(Some("391"), None)), None);
        assert_eq!(antigravity_step_payload_reasoning(&[]), None);
    }

    #[test]
    fn 추론이_길면_다른_백엔드와_같은_6000자_상한으로_자른다() {
        let text = antigravity_step_payload_reasoning(&step_payload(None, Some(&"가".repeat(6_500)))).unwrap();
        assert_eq!(js_length(&text), 6_000);
        assert!(text.ends_with('…'));
        // UTF-16 단위로 자르므로 대리 쌍 가운데가 잘리면 Node의 짝 없는 대리 문자 자리에 U+FFFD가 남는다.
        let emoji = antigravity_step_payload_reasoning(&step_payload(None, Some(&format!("{}😀😀", "x".repeat(5_998))))).unwrap();
        assert_eq!(emoji, format!("{}\u{fffd}…", "x".repeat(5_998)));
    }

    #[test]
    fn 대화_db에서_응답_단계의_추론만_골라_진행_여부와_함께_읽는다() {
        let root = temporary_root("select");
        {
            let database = Connection::open(join_path(&[&root, &format!("{SESSION_ID}.db")])).unwrap();
            database.execute_batch(STEPS_TABLE).unwrap();
            let insert = "INSERT INTO steps (idx, step_type, status, step_payload) VALUES (?, ?, ?, ?)";
            database.execute(insert, (0, 14, 3, step_payload(Some("사용자 입력"), None))).unwrap();
            database.execute(insert, (1, 15, 3, step_payload(None, Some("먼저 파일을 만들기로 했다.")))).unwrap();
            database.execute(insert, (2, 132, 3, step_payload(None, Some("도구 단계는 무시한다.")))).unwrap();
            database.execute(insert, (3, 15, 8, step_payload(None, Some("검산을 시작한다.")))).unwrap();
            database.execute(insert, (4, 15, 3, step_payload(Some("작업을 마쳤습니다."), None))).unwrap();
        }
        let reasonings = antigravity_step_reasonings(SESSION_ID, None, &root);
        fs::remove_dir_all(&root).unwrap();
        assert_eq!(
            reasonings,
            [reasoning(1.0, "먼저 파일을 만들기로 했다.", true), reasoning(3.0, "검산을 시작한다.", false)]
        );
    }

    // 같은 단계를 여러 번 읽으면 그동안 자란 추론이 그대로 보여야 한다.
    #[test]
    fn 진행_중인_단계의_추론이_자라면_늘어난_원문을_읽는다() {
        let root = temporary_root("grow");
        let database = Connection::open(join_path(&[&root, &format!("{SESSION_ID}.db")])).unwrap();
        database.execute_batch(STEPS_TABLE).unwrap();
        database
            .execute(
                "INSERT INTO steps (idx, step_type, status, step_payload) VALUES (1, 15, 8, ?)",
                [step_payload(None, Some("상자 수를 센다."))],
            )
            .unwrap();
        let first = antigravity_step_reasonings(SESSION_ID, None, &root);
        database
            .execute(
                "UPDATE steps SET status = 3, step_payload = ? WHERE idx = 1",
                [step_payload(None, Some("상자 수를 센다. 그리고 검산했다."))],
            )
            .unwrap();
        let grown = antigravity_step_reasonings(SESSION_ID, None, &root);
        let latest = antigravity_latest_step_index(SESSION_ID, &root);
        // min_step_index보다 작은 단계는 건너뛴다.
        let after_two = antigravity_step_reasonings(SESSION_ID, Some(2.0), &root);
        let after_one = antigravity_step_reasonings(SESSION_ID, Some(1.0), &root);
        drop(database);
        fs::remove_dir_all(&root).unwrap();

        assert_eq!(first, [reasoning(1.0, "상자 수를 센다.", false)]);
        assert_eq!(grown, [reasoning(1.0, "상자 수를 센다. 그리고 검산했다.", true)]);
        assert_eq!(latest, Some(1.0));
        assert_eq!(after_two, []);
        assert_eq!(after_one, [reasoning(1.0, "상자 수를 센다. 그리고 검산했다.", true)]);
    }

    #[test]
    fn 세션_id나_대화_파일이_없으면_조용히_건너뛴다() {
        let root = temporary_root("missing");
        let latest = [antigravity_latest_step_index("세션 아님", &root), antigravity_latest_step_index(SESSION_ID, &root)];
        let reasonings = [antigravity_step_reasonings("세션 아님", None, &root), antigravity_step_reasonings(SESSION_ID, None, &root)];
        fs::remove_dir_all(&root).unwrap();
        assert_eq!(latest, [None, None]);
        assert_eq!(reasonings, [Vec::new(), Vec::new()]);
    }

    // 기대값은 같은 SQL로 만든 DB에서 Node antigravityStepReasonings·antigravityLatestStepIndex가 낸 결과다.
    // 열 형식이 없는 표라 숫자·문자열·BLOB이 섞이고, SQLite 정렬은 NULL·숫자·문자열·BLOB 순이다.
    #[test]
    fn 형식이_섞인_단계는_node_sqlite_값_변환을_따른다() {
        let root = temporary_root("types");
        let sessions = [
            "CREATE TABLE steps (idx, step_type, status, step_payload);
             INSERT INTO steps VALUES (1, 15, 3, X'a201031a0161'), ('2', 15, '3', X'a201031a0162'), (2.0, 15, 3.0, X'a201031a0163'),
             (4, 15, 8, X'a201031a0164'), (X'05', 15, X'03', X'a201031a0165'), (X'0102', 15, 3, X'a201031a0166'), (7, 15, 3, 12),
             (NULL, 15, NULL, X'a201031a0167'), (9, '15', 3, X'a201031a0168'), (10, 15, 3, CAST(X'a201031a01ff' AS TEXT)),
             (11, 15, ' 3 ', CAST(X'a201031a0169' AS TEXT));",
            "CREATE TABLE steps (idx); INSERT INTO steps VALUES (1), (2.0);",
            "CREATE TABLE steps (idx); INSERT INTO steps VALUES (2.5);",
            "CREATE TABLE steps (idx); INSERT INTO steps VALUES (1), ('x');",
            "CREATE TABLE steps (idx);",
            "CREATE TABLE steps (idx, step_type, status, step_payload); INSERT INTO steps VALUES (9007199254740993, 15, 3, X'a201031a0161');",
            "CREATE TABLE steps (idx); INSERT INTO steps VALUES (9007199254740991);",
            "CREATE TABLE other (idx);",
        ];
        let ids: Vec<String> = (0..sessions.len()).map(|index| format!("11111111-0000-4000-8000-{:012x}", index + 10)).collect();
        for (id, sql) in ids.iter().zip(sessions) {
            Connection::open(join_path(&[&root, &format!("{id}.db")])).unwrap().execute_batch(sql).unwrap();
        }
        let json = |reasonings: Vec<StepReasoning>| String::from_utf8(to_js_json(&reasonings).unwrap()).unwrap();
        let all = json(antigravity_step_reasonings(&ids[0], None, &root));
        let min_four = json(antigravity_step_reasonings(&ids[0], Some(4.0), &root));
        let min_fraction = json(antigravity_step_reasonings(&ids[0], Some(2.5), &root));
        let latest: Vec<Option<f64>> = ids.iter().map(|id| antigravity_latest_step_index(id, &root)).collect();
        let too_large = antigravity_step_reasonings(&ids[5], None, &root);
        fs::remove_dir_all(&root).unwrap();

        let tail = r#"{"stepIndex":4,"text":"d","done":false},{"stepIndex":2,"text":"b","done":true},{"stepIndex":null,"text":"f","done":true},{"stepIndex":5,"text":"e","done":true}"#;
        let head = r#"{"stepIndex":0,"text":"g","done":false},{"stepIndex":1,"text":"a","done":true},{"stepIndex":2,"text":"c","done":true},"#;
        assert_eq!(all, format!("[{head}{tail}]"));
        assert_eq!(min_four, format!("[{tail}]"));
        assert_eq!(min_fraction, all);
        assert_eq!(latest, [None, Some(2.0), None, None, None, None, Some(9_007_199_254_740_991.0), None]);
        assert_eq!(too_large, []);
    }
}
