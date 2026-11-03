// 프롬프트 조립 테스트. Node roleplay.test.mjs의 조립 테스트와 Node 원본으로 뽑은 정규식·슬라이스 경계값을 옮겼다.

use std::cell::RefCell;

use serde_json::{Value, json};

use super::*;
use crate::roleplay::risu_format::fixtures::synthetic_preset_json;

fn synthetic_preset(overrides: Value) -> Value {
    let mut preset = synthetic_preset_json("test-module");
    if let (Value::Object(fields), Value::Object(overrides)) = (&mut preset, overrides) {
        fields.extend(overrides);
    }
    preset
}

fn message(id: &str, role: &str, content: &str) -> ChatMessage {
    ChatMessage { id: id.into(), role: role.into(), content: content.into(), created_at: None }
}

fn three_messages(middle: &str) -> Vec<ChatMessage> {
    vec![message("1", "user", "안녕"), message("2", "char", middle), message("3", "user", "다음")]
}

struct Assemble<'a> {
    preset: &'a Value,
    toggles: Value,
    character_description: &'a str,
    first_message: &'a str,
    speaker_cue: bool,
    messages: &'a [ChatMessage],
    drop_oldest: usize,
}

impl<'a> Assemble<'a> {
    fn new(preset: &'a Value, messages: &'a [ChatMessage]) -> Self {
        Self { preset, toggles: json!({}), character_description: "", first_message: "", speaker_cue: false, messages, drop_oldest: 0 }
    }

    fn run(&self) -> Vec<FormattedItem> {
        assemble_roleplay_messages(&AssembleInput {
            preset: self.preset,
            modules: &[],
            toggles: &self.toggles,
            character_name: "코대리",
            character_description: self.character_description,
            first_message: self.first_message,
            user_name: "민지",
            persona: "",
            speaker_cue: self.speaker_cue,
            messages: self.messages,
            drop_oldest: self.drop_oldest,
        })
        .expect("조립")
    }
}

fn script(kind: &str, pattern: Value, out: Value, flag: Value, able_flag: bool) -> Value {
    json!({ "type": kind, "in": pattern, "out": out, "flag": flag, "ableFlag": able_flag })
}

#[test]
fn restores_escaped_characters_before_sending() {
    let preset = json!({ "promptTemplate": [{ "type": "plain", "role": "system", "text": "규칙 {{#escape}}{bkspc}{{/}}" }] });
    let formatted = Assemble::new(&preset, &[]).run();
    assert_eq!(formatted[0].content, "규칙 {bkspc}");
}

#[test]
fn reads_toggle_definitions_by_kind() {
    let toggles = parse_toggle_definitions(Some(&json!("on=켜기\nmode=방식=select=짧게,길게\n=묶음=group\nmemo=메모=text")));
    assert_eq!(
        serde_json::to_value(toggles).unwrap(),
        json!([
            { "key": "on", "value": "켜기", "type": "checkbox", "options": [] },
            { "key": "mode", "value": "방식", "type": "select", "options": ["짧게", "길게"] },
            { "key": "", "value": "묶음", "type": "group", "options": [] },
            { "key": "memo", "value": "메모", "type": "text", "options": [] },
        ])
    );
    assert!(parse_toggle_definitions(None).is_empty());
    assert!(parse_toggle_definitions(Some(&json!(""))).is_empty());
}

#[test]
fn regex_scripts_keep_group_substitution_and_order() {
    let scripts = [
        script("editoutput", json!("a"), json!("b"), json!("g"), false),
        script("editoutput", json!(r"(\d+)"), json!("[$1]"), json!("g<order 5>"), true),
        script("editinput", json!("x"), json!("y"), json!("g"), false),
    ];
    let context = CbsContext::default();
    assert_eq!(apply_regex_scripts(&scripts, "a1 a22", "editoutput", &context).unwrap(), "b[1] b[22]");
    assert_eq!(apply_regex_scripts(&scripts, "x", "editinput", &context).unwrap(), "y");
}

#[test]
fn regex_script_edges_match_node() {
    let s = |pattern: &str, out: &str, flag: &str| script("editoutput", json!(pattern), json!(out), json!(flag), true);
    let cases: Vec<(&str, Vec<Value>, &str, &str)> = vec![
        ("moveTop", vec![s(r"(\w+)!", "@@move_top <$1>", "g")], "a b! c d!", "<b>\n\na  c d!"),
        ("moveBottomAction", vec![s(r"x(?<n>\d)", "[$<n>|$&|$9|$$1]", "g<move_bottom,no_end_nl>")], "x1 y x2", " y x2\n[1|x1|$9|$$1]"),
        ("moveNonGlobal", vec![s(r"(\d)", "@@move_bottom #$1", "")], "1 2 3", " 2 3\n#1"),
        ("sticky", vec![s("a", "@@move_top A", "y")], "aab", "A\nab"),
        ("cbsAction", vec![s("{{user}}", "<{{user}}>", "g<cbs>")], "민지 민지", "<민지>\n <민지>\n"),
        ("emo", vec![s("a", "@@emo x", "g")], "aaa", "aaa"),
        ("injectAction", vec![s("a", "b", "g<inject>")], "aaa", "aaa"),
        ("badRegex", vec![s("(", "x", "g"), s("a", "b", "g")], "aa", "bb"),
        ("badFlags", vec![s("a", "b", "uv")], "aa", "aa"),
        ("dollarN", vec![s("a", "1$n2", "g")], "a", "1\n2"),
        ("endGt", vec![s("a", "<b>", "g")], "a a", "<b>\n <b>\n"),
        (
            "ignoreCase",
            vec![s("A", "x", "gi<order 3>"), s("x", "y", "g<order x>"), s("y", "z", "g<order 9>")],
            "aA",
            "zz",
        ),
        ("unable", vec![script("editoutput", json!("a"), json!("b"), json!("i"), false)], "aA", "bA"),
        ("emptyFlag", vec![s("a", "b", "")], "aa", "bb"),
        ("dataToken", vec![s("a", "[{{data}}]", "g")], "a", "[{{data}}]"),
        ("numberIn", vec![script("editoutput", json!(5), json!("five"), json!("g"), true)], "555", "fivefivefive"),
        ("nullOut", vec![script("editoutput", json!("a"), Value::Null, json!("g"), true)], "bab", "bb"),
    ];
    let context = CbsContext { user_name: Some("민지"), ..Default::default() };
    for (name, scripts, input, expected) in cases {
        assert_eq!(apply_regex_scripts(&scripts, input, "editoutput", &context).unwrap(), expected, "{name}");
    }
    let number_flag = [script("editoutput", json!("a"), json!("b"), json!(5), true)];
    assert_eq!(
        apply_regex_scripts(&number_flag, "aa", "editoutput", &context),
        Err(JsError("script.flag?.includes is not a function".into()))
    );
    assert!(apply_regex_scripts(&[Value::Null], "a", "editoutput", &context).is_err());
}

#[test]
fn assembles_blocks_in_preset_order_with_model_flags() {
    let preset = synthetic_preset(json!({}));
    let messages = three_messages("반가워 비밀");
    let formatted = Assemble { toggles: json!({ "on": "1" }), character_description: "명랑한 개발자", ..Assemble::new(&preset, &messages) }.run();
    let body = roleplay_request_body(&preset, "test-model", &formatted).unwrap();
    assert_eq!(
        body,
        json!({
            "model": "test-model",
            "max_tokens": 1000,
            "stream": false,
            "temperature": 0.8,
            "thinking": { "type": "enabled", "budget_tokens": 500, "display": "summarized" },
            "system": "시작 켜짐\n\n<설명>명랑한 개발자</설명>\n\n[Start a new chat]",
            "messages": [
                { "role": "user", "content": "안녕" },
                { "role": "assistant", "content": "반가워 (가림)" },
                { "role": "user", "content": "system: 중간 지시 민지\n\n다음" },
                { "role": "assistant", "content": "알겠어요" },
            ],
        })
    );
    let keys: Vec<&str> = body.as_object().unwrap().keys().map(String::as_str).collect();
    assert_eq!(keys, ["model", "messages", "max_tokens", "stream", "system", "temperature", "thinking"]);
}

#[test]
fn without_custom_flags_later_system_becomes_user_message() {
    let preset = synthetic_preset(json!({ "enableCustomFlags": false, "thinkingType": "off" }));
    let messages = three_messages("반가워");
    let formatted = Assemble::new(&preset, &messages).run();
    let body = roleplay_request_body(&preset, "m", &formatted).unwrap();
    assert_eq!(body.get("thinking"), None);
    // 리스도 설명이 비어 있으면 innerFormat만 남은 블록을 그대로 보낸다.
    assert_eq!(body["system"], json!("시작 \n\n<설명></설명>\n\n[Start a new chat]"));
    assert_eq!(
        body["messages"],
        json!([
            { "role": "user", "content": "안녕" },
            { "role": "assistant", "content": "반가워" },
            { "role": "user", "content": "System: 중간 지시 민지\n\n다음" },
            { "role": "assistant", "content": "알겠어요" },
        ])
    );
}

#[test]
fn speaker_cue_goes_after_last_chat_unless_cueinput_is_on() {
    let preset = synthetic_preset(json!({}));
    let messages = three_messages("반가워");
    let plain = Assemble::new(&preset, &messages).run();
    let cued = Assemble { speaker_cue: true, ..Assemble::new(&preset, &messages) }.run();
    let at = cued.iter().position(|item| item.content == SPEAKER_CUE_PROMPT).unwrap();
    assert_eq!(cued.len(), plain.len() + 1);
    assert_eq!(cued[at].role, json!("system"));
    assert_eq!(cued[at - 1].content, "다음");
    let mut spliced = cued.clone();
    spliced.remove(at);
    assert_eq!(spliced, plain);
    // 다른 프리셋에서 켠 cueinput 값이 남아 있어도 지금 프리셋에 그 토글이 없으면 넣는다.
    let stale = Assemble { speaker_cue: true, toggles: json!({ "cueinput": "1" }), ..Assemble::new(&preset, &messages) }.run();
    assert_eq!(stale.len(), plain.len() + 1);
    let cue_preset = synthetic_preset(json!({ "customPromptTemplateToggle": "cueinput=화자 표시" }));
    let with_cue = |speaker_cue| Assemble { speaker_cue, toggles: json!({ "cueinput": "1" }), ..Assemble::new(&cue_preset, &messages) }.run();
    assert_eq!(with_cue(true), with_cue(false));
    let empty = Assemble { speaker_cue: true, ..Assemble::new(&preset, &[]) }.run();
    assert_eq!(empty.last().unwrap().content, SPEAKER_CUE_PROMPT);
}

#[test]
fn drops_oldest_chats_but_keeps_new_chat_marker_and_last_message() {
    let preset = synthetic_preset(json!({ "enableCustomFlags": false, "thinkingType": "off" }));
    let messages =
        vec![message("1", "user", "하나"), message("2", "char", "둘"), message("3", "user", "셋"), message("4", "char", "넷")];
    let chat_texts = ["[Start a new chat]", "첫인사", "하나", "둘", "셋", "넷"];
    let chats_of = |drop_oldest| -> Vec<String> {
        Assemble { first_message: "첫인사", drop_oldest, ..Assemble::new(&preset, &messages) }
            .run()
            .into_iter()
            .map(|item| item.content)
            .filter(|content| chat_texts.contains(&content.as_str()))
            .collect()
    };
    assert_eq!(chats_of(0), ["[Start a new chat]", "첫인사", "하나", "둘", "셋", "넷"]);
    assert_eq!(chats_of(2), ["[Start a new chat]", "둘", "셋", "넷"]);
    assert_eq!(chats_of(99), ["[Start a new chat]", "넷"]);
}

#[tokio::test]
async fn fit_picks_the_request_that_drops_fewest_chats() {
    let counted = RefCell::new(Vec::new());
    // 대화 하나가 10토큰, 프리셋이 5토큰이라고 친다.
    let build = |drop: usize| -> Result<Value, ()> { Ok(json!({ "drop": drop, "tokens": 5 + 10 * 1.max(8_i64 - drop as i64) })) };
    let count = |body: Value| {
        counted.borrow_mut().push(body["drop"].as_u64().unwrap());
        async move { Ok::<f64, ()>(body["tokens"].as_f64().unwrap()) }
    };
    let fit = fit_roleplay_context(build, 9, 1000.0, count).await.unwrap();
    assert_eq!(fit, ContextFit { body: Some(build(0).unwrap()), dropped: 0, tokens: None });
    assert_eq!(*counted.borrow(), [0]);

    counted.borrow_mut().clear();
    let fit = fit_roleplay_context(build, 9, 40.0, count).await.unwrap();
    assert_eq!(fit.dropped, 5);
    assert_eq!(fit.body.unwrap()["tokens"], json!(35));
    assert!(counted.borrow().len() <= 6, "센 횟수 {}", counted.borrow().len());

    let impossible = fit_roleplay_context(build, 9, 10.0, count).await.unwrap();
    assert_eq!(impossible, ContextFit { body: None, dropped: 9, tokens: Some(15.0) });
}

#[test]
fn slices_roles_and_flags_match_node_edges() {
    let preset = json!({
        "promptTemplate": [
            { "type": "plain", "role": "constructor", "text": "P1" },
            { "type": "description", "role2": "nobody", "innerFormat": "[{{slot}}]" },
            { "type": "chat", "rangeStart": -2 },
            { "type": "plain", "role": "user", "text": "U" },
            { "type": "chat", "rangeStart": 1, "rangeEnd": null },
            { "type": "chat", "rangeStart": -1000, "rangeEnd": 1 },
        ],
        "enableCustomFlags": true,
        "customFlags": [9, 10],
        "promptSettings": { "trimStartNewChat": true },
    });
    let messages = vec![message("a", "user", "하나"), message("b", "char", "둘"), message("c", "user", "셋 <Thoughts>속\n생각</Thoughts>끝")];
    let formatted = assemble_roleplay_messages(&AssembleInput {
        preset: &preset,
        modules: &[],
        toggles: &json!({}),
        character_name: "코",
        character_description: "설명 $&",
        first_message: "",
        user_name: "민",
        persona: "",
        speaker_cue: false,
        messages: &messages,
        drop_oldest: 0,
    })
    .unwrap();
    let shown: Vec<(String, &str, Option<&str>)> = formatted
        .iter()
        .map(|item| {
            let role = match &item.role {
                Value::Null => "UNDEFINED".to_string(),
                Value::String(role) if role.starts_with('\u{0}') => "FUNCTION".to_string(),
                role => role.as_str().unwrap().to_string(),
            };
            (role, item.content.as_str(), item.memo.as_deref())
        })
        .collect();
    let expected = [
        ("FUNCTION", "P1", None),
        ("UNDEFINED", "[설명 {{slot}}]", None),
        ("assistant", "둘", Some("b")),
        ("user", "셋 끝", Some("c")),
        ("user", "U", None),
        ("user", "하나", Some("a")),
        ("assistant", "둘", Some("b")),
        ("user", "셋 끝", Some("c")),
    ];
    let expected: Vec<(String, &str, Option<&str>)> = expected.iter().map(|(role, content, memo)| (role.to_string(), *content, *memo)).collect();
    assert_eq!(shown, expected);

    let mut request_preset = preset.clone();
    let fields = request_preset.as_object_mut().unwrap();
    fields.insert("maxResponse".into(), Value::Null);
    fields.insert("temperature".into(), Value::Null);
    fields.insert("thinkingType".into(), json!("budget"));
    fields.insert("thinkingTokens".into(), json!(10));
    let body = roleplay_request_body(&request_preset, "m", &formatted).unwrap();
    assert_eq!(
        serde_json::to_string(&body).unwrap(),
        r#"{"model":"m","messages":[{"role":"user","content":" "},{"role":"assistant","content":"둘"},{"role":"user","content":"셋 끝\nU\n하나"},{"role":"assistant","content":"둘"},{"role":"user","content":"셋 끝"}],"max_tokens":4096,"stream":false,"temperature":0,"thinking":{"type":"enabled","budget_tokens":10,"display":"summarized"}}"#
    );
}

#[test]
fn active_modules_follow_integration_and_enabled_ids() {
    let module = |id: &str, namespace: Value| Asset {
        id: id.into(),
        kind: "module".into(),
        name: id.into(),
        data: json!({ "namespace": namespace }),
        imported_at: String::new(),
    };
    let modules = [module("m1", json!("a")), module("m2", json!("b")), module("m3", json!(5)), module("m4", json!(""))];
    let preset = json!({ "moduleIntergration": " a , 5," });
    let ids: Vec<&str> = active_modules(Some(&preset), &modules, &["m4".to_string()]).iter().map(|module| module.id.as_str()).collect();
    assert_eq!(ids, ["m1", "m4"]);
    assert!(active_modules(None, &modules, &[]).is_empty());
}

#[test]
fn empty_request_is_rejected() {
    assert_eq!(roleplay_request_body(&json!({}), "m", &[]), Err(JsError("보낼 내용이 없습니다.".into())));
    let only_system = [FormattedItem::new("system", "규칙".into(), None)];
    let body = roleplay_request_body(&json!({}), "m", &only_system).unwrap();
    assert_eq!(body["messages"], json!([{ "role": "user", "content": "Start" }]));
    assert_eq!(body.get("system"), None);
}
