// 리스 프리셋의 프롬프트 템플릿·토글·정규식으로 롤플레이 요청을 리스와 같은 순서로 조립한다. Node roleplay-prompt.mjs 이식.
// 원본: RisuAI(GPL-3.0) src/ts/process/index.svelte.ts, scripts.ts, request/anthropic.ts

use std::collections::{HashMap, HashSet};

use serde::Serialize;
use serde_json::{Map, Value, json};

use super::cbs::{CbsContext, parse_cbs, prototype_value, risu_unescape};
use super::js_regex::{JsMatch, JsRegExp, replace_first};
use super::js_sort::js_sort_by;
use super::{Asset, ChatMessage, js_number};
use crate::js::{js_max, js_string, js_to_number, js_trim, js_truthy, parse_int};

/// 조립 중 생긴 JS 오류(TypeError 등). Node에서는 RoleplayError가 아니라 "답을 만들지 못했습니다."가 된다.
#[derive(Debug, Clone, PartialEq)]
pub struct JsError(pub String);

fn type_error(message: &str) -> JsError {
    JsError(message.to_string())
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ToggleDefinition {
    pub key: String,
    pub value: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub options: Vec<String>,
}

pub fn parse_toggle_definitions(template: Option<&Value>) -> Vec<ToggleDefinition> {
    let Some(template) = template.filter(|template| js_truthy(Some(template))) else {
        return Vec::new();
    };
    let mut toggles = Vec::new();
    for line in js_string(template).split('\n') {
        let parts: Vec<&str> = line.split('=').collect();
        let (key, value, kind, option) = (parts[0], parts.get(1).copied(), parts.get(2).copied(), parts.get(3).copied());
        let definition = |value: &str, kind: &str, options: Vec<String>| ToggleDefinition {
            key: key.to_string(),
            value: value.to_string(),
            kind: kind.to_string(),
            options,
        };
        let has_value = value.is_some_and(|value| !value.is_empty());
        match kind {
            Some(kind @ ("group" | "groupEnd" | "divider")) => toggles.push(definition(value.unwrap_or(""), kind, Vec::new())),
            Some("caption") if has_value => toggles.push(definition(value.unwrap_or(""), "caption", Vec::new())),
            _ if !key.is_empty() && has_value => {
                let kind = kind.filter(|kind| matches!(*kind, "select" | "text" | "textarea")).unwrap_or("checkbox");
                let options = option.map(|option| option.split(',').map(str::to_string).collect()).unwrap_or_default();
                toggles.push(definition(value.unwrap_or(""), kind, options));
            }
            _ => {}
        }
    }
    toggles
}

/// 프로필 토글 값 중 문자열만 `toggle_<이름>` 전역 변수로 쓴다.
pub fn toggle_variables(values: &Value) -> HashMap<String, String> {
    let Value::Object(values) = values else {
        return HashMap::new();
    };
    values.iter().filter_map(|(key, value)| Some((format!("toggle_{key}"), value.as_str()?.to_string()))).collect()
}

/// 프리셋의 moduleIntergration(쉼표 구분 네임스페이스)에 든 모듈과 직접 켠 모듈을 쓴다.
pub fn active_modules<'a>(preset: Option<&Value>, modules: &'a [Asset], enabled_module_ids: &[String]) -> Vec<&'a Asset> {
    let integration = match preset.and_then(|preset| preset.get("moduleIntergration")) {
        None | Some(Value::Null) => String::new(),
        Some(value) => js_string(value),
    };
    let namespaces: Vec<&str> = integration.split(',').map(js_trim).filter(|value| !value.is_empty()).collect();
    modules
        .iter()
        .filter(|module| {
            enabled_module_ids.contains(&module.id)
                || matches!(module.data.get("namespace"), Some(Value::String(namespace)) if !namespace.is_empty() && namespaces.contains(&namespace.as_str()))
        })
        .collect()
}

/// `[...value]`. 문자열은 글자마다, 배열은 항목마다 펼치고 그 밖의 값은 TypeError다.
fn spread_into(value: Option<&Value>, scripts: &mut Vec<Value>) -> Result<(), JsError> {
    match value {
        None | Some(Value::Null) => Ok(()),
        Some(Value::Array(items)) => {
            scripts.extend(items.iter().cloned());
            Ok(())
        }
        Some(Value::String(text)) => {
            scripts.extend(text.chars().map(|c| Value::String(c.to_string())));
            Ok(())
        }
        Some(_) => Err(type_error("regex is not iterable")),
    }
}

pub fn regex_scripts(preset: Option<&Value>, modules: &[&Asset]) -> Result<Vec<Value>, JsError> {
    let mut scripts = Vec::new();
    spread_into(preset.and_then(|preset| preset.get("regex")), &mut scripts)?;
    for module in modules {
        spread_into(module.data.get("regex"), &mut scripts)?;
    }
    Ok(scripts)
}

#[derive(Clone)]
struct ParsedScript {
    script: Value,
    order: f64,
    actions: Vec<String>,
}

fn regex(pattern: &str, flags: &str) -> JsRegExp {
    JsRegExp::new(pattern, flags).unwrap_or_else(|error| panic!("고정 정규식 {pattern}: {error}"))
}

fn parsed_scripts(scripts: &[Value]) -> Result<Vec<ParsedScript>, JsError> {
    let mut order_changed = false;
    let mut result = Vec::new();
    for script in scripts {
        if script.is_null() {
            return Err(type_error("Cannot read properties of null (reading 'ableFlag')"));
        }
        let has_meta = js_truthy(script.get("ableFlag"))
            && match script.get("flag") {
                None | Some(Value::Null) => false,
                Some(Value::String(flag)) => flag.contains('<'),
                Some(Value::Array(items)) if items.iter().any(|item| item.as_str() == Some("<")) => {
                    return Err(type_error("script.flag.replace is not a function"));
                }
                Some(Value::Array(_)) => false,
                Some(_) => return Err(type_error("script.flag?.includes is not a function")),
            };
        if !has_meta {
            result.push(ParsedScript { script: script.clone(), order: 0.0, actions: Vec::new() });
            continue;
        }
        let mut order = 0.0;
        let mut actions = Vec::new();
        let flag = script.get("flag").and_then(Value::as_str).unwrap_or_default();
        let flag = regex("<(.+?)>", "g").replace_with(flag, |found| {
            let meta = found.captures.get(1).cloned().flatten().unwrap_or_default();
            for item in meta.split(',').map(js_trim) {
                if let Some(rest) = item.strip_prefix("order ") {
                    order = parse_int(rest);
                    order_changed = true;
                } else {
                    actions.push(item.to_string());
                }
            }
            String::new()
        });
        let mut script = script.clone();
        if let Value::Object(fields) = &mut script {
            fields.insert("flag".into(), Value::String(flag));
        }
        result.push(ParsedScript { script, order, actions });
    }
    if order_changed {
        // order에 NaN이 섞이면 비교가 어긋나므로 V8 정렬을 그대로 따른다(운영 자산에 실제로 있다).
        js_sort_by(&mut result, |a, b| b.order - a.order);
    }
    Ok(result)
}

fn replace_match_groups(template: &str, matched: &JsMatch) -> String {
    let numbered = regex(r"(?<!\$)\$[0-9]+", "g").replace_with(template, |found| {
        let value = found.matched();
        let index = parse_int(&value[1..]);
        if index < matched.captures.len() as f64 {
            matched.captures[index as usize].clone().unwrap_or_else(|| "undefined".into())
        } else {
            value.to_string()
        }
    });
    let whole = regex(r"\$&", "g").replace(&numbered, matched.matched());
    regex(r"(?<!\$)\$<([^>]+)>", "g").replace_with(&whole, |found| {
        let name = found.captures.get(1).cloned().flatten().unwrap_or_default();
        matched.group(&name).map_or_else(|| found.matched().to_string(), str::to_string)
    })
}

/// processScriptFull의 정규식 단계. 감정·주입·되풀이 같은 리스 화면 전용 동작은 다루지 않는다.
pub fn apply_regex_scripts(scripts: &[Value], input: &str, mode: &str, context: &CbsContext) -> Result<String, JsError> {
    let mut data = parse_cbs(input, context);
    for ParsedScript { script, actions, .. } in parsed_scripts(scripts)? {
        if script.get("type").and_then(Value::as_str) != Some(mode) || !js_truthy(script.get("in")) {
            continue;
        }
        // 리스도 깨진 정규식 하나는 건너뛰고 나머지를 적용한다(None).
        if let Some(next) = apply_script(&script, &actions, &data, context) {
            data = next;
        }
    }
    Ok(data)
}

fn apply_script(script: &Value, actions: &[String], data: &str, context: &CbsContext) -> Option<String> {
    let has = |name: &str| actions.iter().any(|action| action == name);
    let mut out = match script.get("out") {
        None | Some(Value::Null) => String::new(),
        Some(value) => js_string(value),
    }
    .replace("$n", "\n");
    let moves = out.starts_with("@@move_top") || out.starts_with("@@move_bottom") || has("move_top") || has("move_bottom");
    let mut flag = if js_truthy(script.get("ableFlag")) {
        match script.get("flag") {
            Some(Value::String(flag)) if !flag.is_empty() => flag.clone(),
            // 문자열이 아닌 참 값이면 replace·trim이 없어 TypeError로 건너뛴다.
            Some(value) if js_truthy(Some(value)) => return None,
            _ => "g".to_string(),
        }
    } else {
        "g".to_string()
    };
    if moves {
        flag = flag.replacen('g', "", 1);
    }
    if out.ends_with('>') && !has("no_end_nl") {
        out.push('\n');
    }
    let mut letters = String::new();
    for c in js_trim(&flag).chars().filter(|c| "dgimsuvy".contains(*c)) {
        if !letters.contains(c) {
            letters.push(c);
        }
    }
    let flag = if letters.is_empty() { "u".to_string() } else { letters };
    let source = script.get("in").map(js_string).unwrap_or_default();
    let pattern = if has("cbs") { parse_cbs(&source, context) } else { source };
    let mut regex = JsRegExp::new(&pattern, &flag).ok()?;

    if !(out.starts_with("@@") || !actions.is_empty()) {
        return Some(parse_cbs(&regex.replace(data, &out), context));
    }
    if !regex.test(data) {
        return Some(data.to_string());
    }
    regex.last_index = 0;
    if moves {
        let matches: Vec<JsMatch> = if flag.contains('g') { regex.match_all(data).ok()? } else { regex.exec(data).into_iter().collect() };
        let mut data = regex.replace(data, "");
        let template = out.replacen("@@move_top ", "", 1).replacen("@@move_bottom ", "", 1);
        let top = out.starts_with("@@move_top") || has("move_top");
        for matched in &matches {
            let moved = replace_match_groups(&template, matched);
            data = if top { format!("{moved}\n{data}") } else { format!("{data}\n{moved}") };
        }
        return Some(data);
    }
    if !out.starts_with("@@emo ") && !out.starts_with("@@inject") && !has("inject") {
        return Some(parse_cbs(&regex.replace(data, &out), context));
    }
    Some(data.to_string())
}

/// 조립한 메시지 하나. role은 JS 값 그대로(undefined는 Null, Object.prototype 함수는 표시 문자열)다.
#[derive(Debug, Clone, PartialEq)]
pub struct FormattedItem {
    pub role: Value,
    pub content: String,
    pub memo: Option<String>,
}

impl FormattedItem {
    fn new(role: &str, content: String, memo: Option<String>) -> Self {
        Self { role: Value::String(role.into()), content, memo }
    }

    fn role_is(&self, role: &str) -> bool {
        self.role.as_str() == Some(role)
    }
}

/// `PROMPT_ROLE[key]`. 빈 객체 리터럴이라 Object.prototype 속성 이름도 값(함수)이 나온다. None은 undefined다.
fn prompt_role(value: Option<&Value>) -> Option<Value> {
    let key = value.map_or_else(|| "undefined".to_string(), js_string);
    match key.as_str() {
        "system" => Some(json!("system")),
        "user" => Some(json!("user")),
        "bot" => Some(json!("assistant")),
        _ => prototype_value(&key).map(|_| Value::String(format!("\u{0}prototype:{key}"))),
    }
}

/// 프로필 토글 값은 프리셋을 바꿔도 남으므로, 지금 프리셋·모듈에 cueinput 토글이 있을 때만 켜진 것으로 본다.
fn has_active_cue_toggle(preset: &Value, modules: &[&Asset], toggles: &Value) -> bool {
    if toggles.get("cueinput").and_then(Value::as_str) != Some("1") {
        return false;
    }
    std::iter::once(preset.get("customPromptTemplateToggle"))
        .chain(modules.iter().map(|module| module.data.get("customModuleToggle")))
        .any(|template| parse_toggle_definitions(template).iter().any(|definition| definition.key == "cueinput"))
}

/// 앱의 "대사에 화자 이름표" 설정. 소악마 cueinput과 같은 형식이라 앱 화면이 화자를 구분할 수 있다.
pub const SPEAKER_CUE_PROMPT: &str = "<Character_Cue_Guidelines>
Put a Character Cue in front of every line of spoken dialogue so readers instantly know who is talking.
- Format: Gender_Emoji Name: \"Dialogue\" — each line of dialogue on its own line.
- Use only 👩 for female speakers and 👨 for male speakers, and keep each character's name the same throughout the chat.
- Inner thoughts use the same cue with single quotes: Gender_Emoji Name: 'Thought'
- Narration never gets a cue.
</Character_Cue_Guidelines>";

fn remove_positions(text: Option<&Value>) -> String {
    let text = match text {
        None | Some(Value::Null) => String::new(),
        Some(value) => js_string(value),
    };
    regex("{{position::(.+?)}}", "g").replace(&text, "")
}

/// 카드의 rangeStart·rangeEnd. None은 undefined, null은 비교·slice에서 0과 같다.
fn range_value(value: Option<&Value>) -> Option<f64> {
    match value {
        None => None,
        Some(value) => Some(js_to_number(value)),
    }
}

/// `array.slice(start, end)`의 위치 계산(ToIntegerOrInfinity 뒤 길이로 자르기).
fn slice_index(value: Option<f64>, length: f64, fallback: f64) -> usize {
    let Some(value) = value else {
        return fallback as usize;
    };
    let relative = if value.is_nan() { 0.0 } else { value.trunc() };
    let index = if relative < 0.0 { (length + relative).max(0.0) } else { relative.min(length) };
    index as usize
}

fn slice_chats(chats: &[FormattedItem], card: &Value) -> Vec<FormattedItem> {
    let length = chats.len() as f64;
    let mut start = range_value(card.get("rangeStart"));
    let mut end = match card.get("rangeEnd") {
        Some(Value::String(text)) if text == "end" => Some(length),
        other => range_value(other),
    };
    if start == Some(-1000.0) {
        start = Some(0.0);
        end = Some(length);
    }
    if let Some(value) = start.filter(|value| *value < 0.0) {
        start = Some(js_max(0.0, length + value));
    }
    if let Some(value) = end.filter(|value| *value < 0.0) {
        end = Some(js_max(0.0, length + value));
    }
    if let (Some(start), Some(end)) = (start, end)
        && start >= end
    {
        return Vec::new();
    }
    let from = slice_index(start, length, 0.0);
    let to = slice_index(end, length, length);
    if from < to { chats[from..to].to_vec() } else { Vec::new() }
}

pub struct AssembleInput<'a> {
    pub preset: &'a Value,
    pub modules: &'a [&'a Asset],
    pub toggles: &'a Value,
    pub character_name: &'a str,
    pub character_description: &'a str,
    pub first_message: &'a str,
    pub user_name: &'a str,
    pub persona: &'a str,
    pub speaker_cue: bool,
    pub messages: &'a [ChatMessage],
    pub drop_oldest: usize,
}

fn push_items(formatted: &mut Vec<FormattedItem>, items: Vec<FormattedItem>) {
    for item in items {
        if js_trim(&item.content).is_empty() {
            continue;
        }
        match formatted.last_mut() {
            Some(last) if item.role_is("system") && last.role_is("system") && last.memo == item.memo => {
                last.content.push_str("\n\n");
                last.content.push_str(&item.content);
            }
            _ => formatted.push(item),
        }
    }
}

/// 프리셋 블록 순서대로 OpenAI식 메시지 목록을 만든다(리스 sendChat의 템플릿 단계).
pub fn assemble_roleplay_messages(input: &AssembleInput) -> Result<Vec<FormattedItem>, JsError> {
    let preset = input.preset;
    let scripts = regex_scripts(Some(preset), input.modules)?;
    let vars = toggle_variables(input.toggles);
    let base = CbsContext {
        user_name: Some(input.user_name),
        char_name: Some(input.character_name),
        vars: Some(&vars),
        last_message_index: Some(input.messages.len() as f64 - 1.0),
        messages: Some(input.messages),
        ..Default::default()
    };
    let parse = |text: &str| parse_cbs(text, &base);

    let description = FormattedItem::new("system", parse(input.character_description), None);
    let persona_prompts =
        if js_trim(input.persona).is_empty() { Vec::new() } else { vec![FormattedItem::new("system", parse(input.persona), None)] };

    let mut chats = Vec::new();
    if !js_truthy(preset.get("promptSettings").and_then(|settings| settings.get("trimStartNewChat"))) {
        chats.push(FormattedItem::new("system", "[Start a new chat]".into(), Some("NewChat".into())));
    }
    if !input.first_message.is_empty() {
        let context = CbsContext { chat_index: Some(-1.0), ..base };
        let content = apply_regex_scripts(&scripts, &parse(input.first_message), "editprocess", &context)?;
        chats.push(FormattedItem::new("assistant", content, None));
    }
    let mut thoughts = regex(r"<Thoughts>(.+)<\/Thoughts>", "gms");
    for (index, message) in input.messages.iter().enumerate() {
        let context = CbsContext { chat_index: Some(index as f64), ..base };
        let content = apply_regex_scripts(&scripts, &parse(&message.content), "editprocess", &context)?;
        let content = thoughts.replace(&content, "");
        let role = if message.role == "user" { "user" } else { "assistant" };
        chats.push(FormattedItem::new(role, content, Some(message.id.clone())));
    }
    let all_chats: Vec<FormattedItem> = chats.into_iter().filter(|chat| !js_trim(&chat.content).is_empty()).collect();
    // 한도를 넘으면 오래된 대화부터 뺀다. 새 대화 표시와 마지막 메시지는 남긴다.
    let head = usize::from(all_chats.first().is_some_and(|chat| chat.memo.as_deref() == Some("NewChat")));
    let drop = input.drop_oldest.min(all_chats.len().saturating_sub(head + 1));
    let history: Vec<FormattedItem> = all_chats[..head].iter().chain(&all_chats[head + drop..]).cloned().collect();

    let with_role = |items: Vec<FormattedItem>, role: Option<&Value>| -> Vec<FormattedItem> {
        if !js_truthy(role) {
            return items;
        }
        let role = prompt_role(role).unwrap_or(Value::Null);
        items.into_iter().map(|item| FormattedItem { role: role.clone(), ..item }).collect()
    };
    let with_inner_format = |items: Vec<FormattedItem>, inner_format: Option<&Value>| -> Vec<FormattedItem> {
        if !js_truthy(inner_format) || items.is_empty() {
            return items;
        }
        items
            .into_iter()
            .map(|item| {
                let template = parse(&remove_positions(inner_format));
                FormattedItem { content: replace_first(&template, "{{slot}}", &item.content), ..item }
            })
            .collect()
    };

    let mut formatted = Vec::new();
    let cards: Vec<Value> = match preset.get("promptTemplate") {
        None | Some(Value::Null) => Vec::new(),
        Some(value) => {
            let mut cards = Vec::new();
            spread_into(Some(value), &mut cards).map_err(|_| type_error("promptTemplate is not iterable"))?;
            cards
        }
    };
    for card in &cards {
        if card.is_null() {
            return Err(type_error("Cannot read properties of null (reading 'type')"));
        }
        match card.get("type").and_then(Value::as_str) {
            Some("persona") => {
                push_items(&mut formatted, with_inner_format(with_role(persona_prompts.clone(), card.get("role2")), card.get("innerFormat")));
            }
            Some("description") => {
                push_items(&mut formatted, with_inner_format(with_role(vec![description.clone()], card.get("role2")), card.get("innerFormat")));
            }
            Some("plain") => {
                let role = prompt_role(card.get("role")).unwrap_or_else(|| json!("system"));
                let content = parse(&remove_positions(card.get("text")));
                push_items(&mut formatted, vec![FormattedItem { role, content, memo: None }]);
            }
            Some("chat") => push_items(&mut formatted, slice_chats(&history, card)),
            // jailbreak·cot는 쓰지 않는다. 장기 기억·로어북·작가 노트는 1단계에서 비어 있다. cache는 문자열 요청에서 쓰이지 않는다.
            _ => {}
        }
    }
    // 프리셋이 이미 화자 표시(cueinput)를 켰으면 겹쳐 넣지 않는다.
    // 마지막 대화 바로 뒤에 넣어 프리셋 끝의 지시·이어쓰기 블록 앞에 둔다.
    if input.speaker_cue && !has_active_cue_toggle(preset, input.modules, input.toggles) {
        let ids: HashSet<&str> = input.messages.iter().map(|message| message.id.as_str()).filter(|id| !id.is_empty()).collect();
        let last = formatted.iter().rposition(|item| item.memo.as_deref().is_some_and(|memo| ids.contains(memo)));
        let at = last.map_or(formatted.len(), |index| index + 1);
        formatted.insert(at, FormattedItem::new("system", SPEAKER_CUE_PROMPT.into(), None));
    }
    // 리스 requestChatData처럼 보내기 직전에 #escape로 숨긴 글자를 되돌린다.
    Ok(formatted.into_iter().map(|item| FormattedItem { content: risu_unescape(js_trim(&item.content)), ..item }).collect())
}

/// requestClaude의 Anthropic 변환. 앞쪽 system은 system으로, 이후 system은 "System: "을 붙인 user로 보낸다.
pub fn anthropic_payload(formatted: &[FormattedItem]) -> Result<(String, Vec<(String, String)>), JsError> {
    let mut system = String::new();
    let mut messages: Vec<(String, String)> = Vec::new();
    let add = |messages: &mut Vec<(String, String)>, role: &str, content: &str| match messages.last_mut() {
        Some(last) if last.0 == role => {
            last.1.push_str("\n\n");
            last.1.push_str(content);
        }
        _ => messages.push((role.to_string(), content.to_string())),
    };
    for item in formatted {
        match item.role.as_str() {
            Some(role @ ("user" | "assistant")) => add(&mut messages, role, &item.content),
            Some("system") if messages.is_empty() => {
                system.push_str("\n\n");
                system.push_str(&item.content);
            }
            Some("system") => add(&mut messages, "user", &format!("System: {}", item.content)),
            _ => {}
        }
    }
    if messages.is_empty() {
        if system.is_empty() {
            return Err(JsError("보낼 내용이 없습니다.".into()));
        }
        messages.push(("user".into(), "Start".into()));
        system.clear();
    }
    if messages[0].0 != "user" {
        messages.insert(0, ("user".into(), "Start".into()));
    }
    Ok((js_trim(&system).to_string(), messages))
}

/// 리스 LLMFlags 번호. 사용자 지정 플래그가 꺼져 있으면 Custom API 기본값([6, 8])을 쓴다.
const FLAG_FULL_SYSTEM: f64 = 6.0;
const FLAG_FIRST_SYSTEM: f64 = 7.0;
const FLAG_ALTERNATE_ROLE: f64 = 9.0;
const FLAG_START_WITH_USER: f64 = 10.0;

/// request.ts의 reformater. 앞쪽 system만 남기고 나머지 system은 "system: "을 붙여 다른 역할로 바꾼다.
pub fn reformat_for_model_flags(formatted: &[FormattedItem], preset: &Value) -> Result<Vec<FormattedItem>, JsError> {
    let custom = preset.get("customFlags").filter(|flags| js_truthy(preset.get("enableCustomFlags")) && flags.is_array());
    let flags: Vec<Value> = custom.and_then(Value::as_array).cloned().unwrap_or_else(|| vec![json!(6), json!(8)]);
    let has = |flag: f64| flags.iter().any(|value| value.as_f64() == Some(flag));
    let mut items = formatted.to_vec();
    let mut system_prompt: Option<FormattedItem> = None;
    if !has(FLAG_FULL_SYSTEM) {
        if has(FLAG_FIRST_SYSTEM) {
            while items.first().is_some_and(|item| item.role_is("system")) {
                let first = items.remove(0);
                match &mut system_prompt {
                    Some(prompt) => {
                        prompt.content.push_str("\n\n");
                        prompt.content.push_str(&first.content);
                    }
                    None => system_prompt = Some(first),
                }
            }
        }
        let replacement = preset.get("systemContentReplacement").filter(|value| js_truthy(Some(value)));
        let role = preset.get("systemRoleReplacement").filter(|value| js_truthy(Some(value))).cloned().unwrap_or_else(|| json!("user"));
        for item in items.iter_mut().filter(|item| item.role_is("system")) {
            item.content = match replacement {
                Some(Value::String(replacement)) => replace_first(replacement, "{{slot}}", &item.content),
                Some(_) => return Err(type_error("preset.systemContentReplacement.replace is not a function")),
                None => format!("system: {}", item.content),
            };
            item.role = role.clone();
        }
    }
    if has(FLAG_ALTERNATE_ROLE) {
        let mut merged: Vec<FormattedItem> = Vec::new();
        for item in items {
            match merged.last_mut() {
                Some(last) if last.role == item.role => {
                    last.content.push('\n');
                    last.content.push_str(&item.content);
                }
                _ => merged.push(item),
            }
        }
        items = merged;
    }
    if has(FLAG_START_WITH_USER) && !items.first().is_some_and(|item| item.role_is("user")) {
        items.insert(0, FormattedItem::new("user", " ".into(), None));
    }
    if let Some(prompt) = system_prompt {
        items.insert(0, prompt);
    }
    Ok(items)
}

/// 리스의 최대 컨텍스트처럼, 한도 안에 드는 요청 중 오래된 대화를 가장 적게 뺀 것을 고른다.
/// 뺄수록 토큰이 줄어드므로 이분 탐색으로 세는 횟수를 줄인다. 다 빼도 넘으면 body가 None이다.
#[derive(Debug, Clone, PartialEq)]
pub struct ContextFit {
    pub body: Option<Value>,
    pub dropped: usize,
    pub tokens: Option<f64>,
}

/// count_tokens는 본문을 값으로 받는다. 빌린 값을 받는 비동기 클로저는 tokio::spawn 안에서 Send 증명이 막힌다.
pub async fn fit_roleplay_context<E, F: Future<Output = Result<f64, E>>>(
    build: impl Fn(usize) -> Result<Value, E>,
    max_drop: usize,
    limit: f64,
    mut count_tokens: impl FnMut(Value) -> F,
) -> Result<ContextFit, E> {
    let full = build(0)?;
    if count_tokens(full.clone()).await? <= limit {
        return Ok(ContextFit { body: Some(full), dropped: 0, tokens: None });
    }
    let tokens = count_tokens(build(max_drop)?).await?;
    if tokens > limit {
        return Ok(ContextFit { body: None, dropped: max_drop, tokens: Some(tokens) });
    }
    let mut low = 1;
    let mut high = max_drop;
    while low < high {
        let middle = (low + high) / 2;
        if count_tokens(build(middle)?).await? <= limit {
            high = middle;
        } else {
            low = middle + 1;
        }
    }
    Ok(ContextFit { body: Some(build(high)?), dropped: high, tokens: None })
}

pub fn roleplay_request_body(preset: &Value, model: &str, formatted: &[FormattedItem]) -> Result<Value, JsError> {
    let (system, messages) = anthropic_payload(&reformat_for_model_flags(formatted, preset)?)?;
    let number = |key: &str| preset.get(key).map_or(f64::NAN, js_to_number);
    let max_response = number("maxResponse");
    let max_tokens = if max_response > 0.0 { max_response } else { 4096.0 };
    let mut body = Map::new();
    body.insert("model".into(), json!(model));
    body.insert(
        "messages".into(),
        Value::Array(messages.into_iter().map(|(role, content)| json!({ "role": role, "content": content })).collect()),
    );
    body.insert("max_tokens".into(), js_number(max_tokens));
    body.insert("stream".into(), json!(false));
    if !system.is_empty() {
        body.insert("system".into(), json!(system));
    }
    let temperature = number("temperature");
    if temperature.is_finite() && temperature >= 0.0 {
        body.insert("temperature".into(), js_number(temperature / 100.0));
    }
    let budget = number("thinkingTokens");
    if preset.get("thinkingType").and_then(Value::as_str) == Some("budget") && budget > 0.0 && budget < max_tokens {
        body.insert("thinking".into(), json!({ "type": "enabled", "budget_tokens": js_number(budget), "display": "summarized" }));
    }
    Ok(Value::Object(body))
}

#[cfg(test)]
mod tests;
