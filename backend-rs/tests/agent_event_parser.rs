// Node agent-event-parser.test.mjs가 부른 호출을 녹화한 고정 자료로 Rust 파서의 결과 JSON 바이트를 맞춘다.
// 고정 자료는 tests/fixtures/record-node-calls.mjs로 다시 만들 수 있다.

use officestra_backend::agent_event_parser::{decode_agent_response, parse_agent_event};
use officestra_backend::js::to_js_json;
use serde::Serialize;
use serde_json::Value;

fn json_text(value: &impl Serialize) -> String {
    String::from_utf8(to_js_json(value).unwrap()).unwrap()
}

#[test]
fn node_test_calls_produce_the_same_json() {
    let fixture: Value = serde_json::from_str(include_str!("fixtures/agent-event-parser-cases.json")).expect("Node 녹화 자료");
    let cases = fixture["cases"].as_array().unwrap();
    assert_eq!(cases.len(), 97, "Node 시험 66개가 부른 호출 수");
    let mut mismatches = Vec::new();
    for (index, case) in cases.iter().enumerate() {
        let args = case["args"].as_array().unwrap();
        let text = |position: usize| args.get(position).and_then(Value::as_str);
        let actual = match case["fn"].as_str().unwrap() {
            "parseAgentEvent" => parse_agent_event(text(0).unwrap(), text(1).unwrap(), text(2)).map(|event| json_text(&event)),
            "decodeAgentResponse" => Ok(json_text(&decode_agent_response(text(0).unwrap()))),
            other => panic!("모르는 함수 {other}"),
        };
        let expected = match case.get("error") {
            Some(error) => Err(error.as_str().unwrap().to_string()),
            None => Ok(case["result"].as_str().unwrap().to_string()),
        };
        if actual != expected {
            mismatches.push(format!("#{index} {}({})\n  Node {expected:?}\n  Rust {actual:?}", case["fn"], json_text(args)));
        }
    }
    assert!(mismatches.is_empty(), "{}건 불일치\n{}", mismatches.len(), mismatches.join("\n"));
}
