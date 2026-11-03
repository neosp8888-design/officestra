// 리스 CBS({{…}}) 문법 중 누렁이·소악마 프리셋과 모듈이 쓰는 부분을 리스와 같은 규칙으로 해석한다. Node roleplay-cbs.mjs 이식.
// 원본: RisuAI(GPL-3.0) src/ts/parser/parser.svelte.ts, src/ts/cbs.ts, src/ts/process/infunctions.ts

use std::collections::HashMap;

use serde_json::Value;

use super::ChatMessage;
use super::js_regex::replace_all;
use crate::js::{
    date_parse, is_js_integer, is_js_whitespace, js_length, js_trim, now_milliseconds, number_to_string, parse_js_json,
    string_to_number, to_js_json,
};

/// CBS 함수가 읽는 값. Node의 context 객체에서 쓰는 칸만 둔다.
#[derive(Clone, Copy, Default)]
pub struct CbsContext<'a> {
    pub user_name: Option<&'a str>,
    pub char_name: Option<&'a str>,
    pub vars: Option<&'a HashMap<String, String>>,
    pub chat_index: Option<f64>,
    pub last_message_index: Option<f64>,
    pub messages: Option<&'a [ChatMessage]>,
    /// 시험용 현재 시각(ms). 없으면 실제 시각이다.
    pub now: Option<f64>,
    /// 시험용 난수. 없으면 Math.random처럼 [0, 1) 난수를 쓴다.
    pub random: Option<&'a dyn Fn() -> f64>,
}

/// `Math.random()`. 운영체제 난수 53비트로 만든다.
fn math_random() -> f64 {
    let mut value = 0_u64;
    // SAFETY: 8바이트 정수 하나를 운영체제 난수로 채운다.
    unsafe { libc::arc4random_buf((&raw mut value).cast(), std::mem::size_of::<u64>()) };
    (value >> 11) as f64 / (1_u64 << 53) as f64
}

/// `parseFloat(text)`. 앞 공백 뒤 가장 긴 십진 숫자 앞부분만 읽고, 없으면 NaN이다.
fn parse_float(text: &str) -> f64 {
    let text = text.trim_start_matches(is_js_whitespace);
    let bytes = text.as_bytes();
    let start = usize::from(matches!(bytes.first(), Some(b'+' | b'-')));
    if text[start..].starts_with("Infinity") {
        return if bytes.first() == Some(&b'-') { f64::NEG_INFINITY } else { f64::INFINITY };
    }
    let digits = |from: usize| bytes.get(from..).map_or(0, |rest| rest.iter().take_while(|byte| byte.is_ascii_digit()).count());
    let integer = digits(start);
    let mut end = start + integer;
    let mut fraction = 0;
    if bytes.get(end) == Some(&b'.') {
        fraction = digits(end + 1);
        if integer > 0 || fraction > 0 {
            end += 1 + fraction;
        }
    }
    if integer == 0 && fraction == 0 {
        return f64::NAN;
    }
    if matches!(bytes.get(end), Some(b'e' | b'E')) {
        let mut exponent = end + 1;
        if matches!(bytes.get(exponent), Some(b'+' | b'-')) {
            exponent += 1;
        }
        let count = digits(exponent);
        if count > 0 {
            end = exponent + count;
        }
    }
    text[..end].parse::<f64>().unwrap_or(f64::NAN)
}

/// 연산자의 (우선순위, 왼쪽 결합 여부).
fn operator(c: char) -> Option<(u8, bool)> {
    match c {
        '+' | '-' => Some((2, true)),
        '*' | '/' | '%' => Some((3, true)),
        '^' => Some((4, true)),
        '<' | '>' | '|' | '&' | '≤' | '≥' | '=' | '≠' => Some((1, true)),
        '!' => Some((5, false)),
        _ => None,
    }
}

fn operator_token(token: &str) -> Option<char> {
    let mut chars = token.chars();
    let c = chars.next()?;
    (chars.next().is_none() && operator(c).is_some()).then_some(c)
}

/// `Boolean(parseFloat(token)) || token === "0"`. "0.0"이나 "-0"은 숫자로 보지 않는다(리스와 같다).
fn is_number_token(token: &str) -> bool {
    let number = parse_float(token);
    (number != 0.0 && !number.is_nan()) || token == "0"
}

fn to_rpn(source: &str) -> String {
    let expression: Vec<char> = source.chars().filter(|c| !is_js_whitespace(*c)).collect();
    let mut tokens = Vec::new();
    let mut last = String::new();
    for (index, &c) in expression.iter().enumerate() {
        if c == '-' && (index == 0 || operator(expression[index - 1]).is_some() || expression[index - 1] == '(') {
            last.push(c);
        } else if operator(c).is_some() {
            tokens.push(if last.is_empty() { "0".to_string() } else { std::mem::take(&mut last) });
            tokens.push(c.to_string());
        } else {
            last.push(c);
        }
    }
    tokens.push(if last.is_empty() { "0".to_string() } else { last });

    let mut output = String::new();
    let mut stack: Vec<char> = Vec::new();
    for token in &tokens {
        if is_number_token(token) {
            output.push_str(token);
            output.push(' ');
        } else if let Some(c) = operator_token(token) {
            let (precedence, left) = operator(c).unwrap_or((0, true));
            while let Some(&top) = stack.last() {
                let top_precedence = operator(top).map_or(0, |(value, _)| value);
                if (left && precedence <= top_precedence) || (!left && precedence < top_precedence) {
                    output.push(top);
                    output.push(' ');
                    stack.pop();
                } else {
                    break;
                }
            }
            stack.push(c);
        }
    }
    while let Some(top) = stack.pop() {
        output.push(top);
        output.push(' ');
    }
    js_trim(&output).to_string()
}

/// JS `a ** b`. 지수가 NaN이거나 |a|=1에 무한 지수면 NaN이다(Rust powf는 1).
fn js_pow(base: f64, exponent: f64) -> f64 {
    if exponent.is_nan() || (base.abs() == 1.0 && exponent.is_infinite()) {
        return f64::NAN;
    }
    base.powf(exponent)
}

fn truthy(value: Option<f64>) -> bool {
    value.is_some_and(|number| number != 0.0 && !number.is_nan())
}

/// RPN 계산. None은 JS undefined다(빈 스택에서 꺼낸 값).
fn calculate_rpn(expression: &str) -> Option<f64> {
    let mut stack: Vec<Option<f64>> = Vec::new();
    for token in expression.split(' ') {
        if is_number_token(token) {
            stack.push(Some(parse_float(token)));
            continue;
        }
        let b = stack.pop().flatten();
        let a = stack.pop().flatten();
        let (x, y) = (a.unwrap_or(f64::NAN), b.unwrap_or(f64::NAN));
        let flag = |value: bool| Some(if value { 1.0 } else { 0.0 });
        let strict_equal = match (a, b) {
            (None, None) => true,
            (Some(left), Some(right)) => left == right,
            _ => false,
        };
        match token {
            "+" => stack.push(Some(x + y)),
            "-" => stack.push(Some(x - y)),
            "*" => stack.push(Some(x * y)),
            "/" => stack.push(Some(x / y)),
            "^" => stack.push(Some(js_pow(x, y))),
            "%" => stack.push(Some(x % y)),
            "<" => stack.push(flag(x < y)),
            ">" => stack.push(flag(x > y)),
            "|" => stack.push(if truthy(a) { a } else { b }),
            "&" => stack.push(if truthy(a) { b } else { a }),
            "≤" => stack.push(flag(x <= y)),
            "≥" => stack.push(flag(x >= y)),
            "=" => stack.push(flag(strict_equal)),
            "≠" => stack.push(flag(!strict_equal)),
            "!" => stack.push(flag(!truthy(b))),
            _ => {}
        }
    }
    if stack.is_empty() { Some(0.0) } else { stack.pop().flatten() }
}

/// `text.replace(/<sigil>([a-zA-Z0-9_]+)/g, (_, key) => …)`.
fn replace_sigil(text: &str, sigil: char, mut replace: impl FnMut(&str) -> String) -> String {
    let mut output = String::new();
    let mut rest = text;
    while let Some(index) = rest.find(sigil) {
        output.push_str(&rest[..index]);
        let after = &rest[index + sigil.len_utf8()..];
        let end = after.find(|c: char| !(c.is_ascii_alphanumeric() || c == '_')).unwrap_or(after.len());
        if end == 0 {
            output.push(sigil);
        } else {
            output.push_str(&replace(&after[..end]));
        }
        rest = &after[end..];
    }
    output.push_str(rest);
    output
}

/// `/null/gi` → "0". i 플래그(u 없음)는 ASCII 밖 글자를 ASCII로 접지 않는다.
fn replace_null(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut output = String::new();
    let mut copied = 0;
    let mut index = 0;
    while index + 4 <= bytes.len() {
        if bytes[index..index + 4].eq_ignore_ascii_case(b"null") {
            output.push_str(&text[copied..index]);
            output.push('0');
            index += 4;
            copied = index;
        } else {
            index += 1;
        }
    }
    output.push_str(&text[copied..]);
    output
}

fn execute_calculation(text: &str, context: &CbsContext) -> Option<f64> {
    let numeric = |value: String| {
        let parsed = parse_float(&value);
        if parsed.is_nan() { "0".to_string() } else { number_to_string(parsed) }
    };
    let prepared = replace_sigil(text, '$', |_| "0".to_string());
    let prepared = replace_sigil(&prepared, '@', |key| numeric(global_var(context, key)))
        .replace("&&", "&")
        .replace("||", "|")
        .replace("<=", "≤")
        .replace(">=", "≥")
        .replace("==", "=")
        .replace("!=", "≠");
    calculate_rpn(&to_rpn(&replace_null(&prepared)))
}

/// 리스 calcString. 괄호 안부터 계산해 글자로 넣는다. None은 JS undefined다.
pub fn calc_string(text: &str, context: &CbsContext) -> Option<f64> {
    let mut depth = vec![String::new()];
    for c in text.chars() {
        if c == '(' {
            depth.push(String::new());
        } else if c == ')' && depth.len() > 1 {
            let inner = depth.pop().unwrap_or_default();
            let result = execute_calculation(&inner, context).map_or_else(|| "undefined".to_string(), number_to_string);
            if let Some(last) = depth.last_mut() {
                last.push_str(&result);
            }
        } else if let Some(last) = depth.last_mut() {
            last.push(c);
        }
    }
    execute_calculation(&depth.concat(), context)
}

/// 빈 JS 객체에서 key를 읽을 때 Object.prototype에서 나오는 값의 String().
pub(super) fn prototype_value(key: &str) -> Option<String> {
    match key {
        "__proto__" => Some("[object Object]".to_string()),
        "constructor" => Some("function Object() { [native code] }".to_string()),
        "__defineGetter__" | "__defineSetter__" | "hasOwnProperty" | "__lookupGetter__" | "__lookupSetter__" | "isPrototypeOf"
        | "propertyIsEnumerable" | "toString" | "valueOf" | "toLocaleString" => Some(format!("function {key}() {{ [native code] }}")),
        _ => None,
    }
}

/// `context.vars?.[key] ?? "null"`.
fn global_var(context: &CbsContext, key: &str) -> String {
    let Some(vars) = context.vars else {
        return "null".to_string();
    };
    vars.get(key).cloned().or_else(|| prototype_value(key)).unwrap_or_else(|| "null".to_string())
}

/// 리스 parseArray. JSON 배열이면 그 항목, 아니면 § 구분 문자열이다. None은 undefined다.
fn parse_array(text: Option<&str>) -> Vec<Value> {
    if let Some(Value::Array(items)) = text.and_then(|text| parse_js_json(text).ok()) {
        return items;
    }
    text.unwrap_or("undefined").split('§').map(|item| Value::String(item.to_string())).collect()
}

fn json_text(value: &impl serde::Serialize) -> String {
    to_js_json(value).ok().and_then(|bytes| String::from_utf8(bytes).ok()).unwrap_or_default()
}

fn random_pick(args: &[&str], random: f64) -> String {
    if args.is_empty() {
        return number_to_string(random);
    }
    let items: Vec<String> = if args.len() == 1 {
        args[0].replace("\\,", "§X").split([':', ',']).map(str::to_string).collect()
    } else {
        args.iter().map(|item| item.to_string()).collect()
    };
    let index = (random * items.len() as f64).floor();
    let item = (is_js_integer(index) && index >= 0.0)
        .then(|| items.get(index as usize))
        .flatten()
        .map_or("undefined", String::as_str);
    item.replace("§X", ",")
}

/// 주사위 개수 상한. Node는 개수만큼 그대로 돌아 무한대면 멈추지 않는다.
const MAX_DICE: f64 = 1_000_000.0;

fn roll_dice(args: &[&str], random: &dyn Fn() -> f64) -> String {
    let Some(first) = args.first() else {
        return "1".to_string();
    };
    let notation: Vec<&str> = first.split('d').collect();
    let number_or = |text: &str, fallback: f64| if text.is_empty() { fallback } else { string_to_number(text) };
    let (count, sides) = match notation.len() {
        2 => (number_or(notation[0], 1.0), number_or(notation[1], 6.0)),
        1 => (1.0, string_to_number(notation[0])),
        _ => (1.0, 6.0),
    };
    if count.is_nan() || sides.is_nan() || count < 1.0 || sides < 1.0 || count > MAX_DICE {
        return "NaN".to_string();
    }
    let mut total = 0.0;
    let mut index = 0.0;
    while index < count {
        total += (random() * sides).floor() + 1.0;
        index += 1.0;
    }
    number_to_string(total)
}

/// 로컬 시각의 날짜 칸. JS Date의 get* 값과 같다.
struct LocalTime {
    year: i64,
    month: i64,
    day: i64,
    weekday: i64,
    hours: i64,
    minutes: i64,
    seconds: i64,
}

/// 유효한 시각값이면 로컬 날짜 칸. JS의 Invalid Date는 None이다.
fn local_time(milliseconds: f64) -> Option<LocalTime> {
    if !milliseconds.is_finite() || milliseconds.abs() > 8.64e15 {
        return None;
    }
    let seconds = (milliseconds / 1000.0).floor() as libc::time_t;
    // SAFETY: tm은 C 구조체라 0으로 채워도 유효하다.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    // SAFETY: 두 포인터 모두 이 함수 안의 지역 변수를 가리킨다.
    if unsafe { libc::localtime_r(&seconds, &mut tm) }.is_null() {
        return None;
    }
    Some(LocalTime {
        year: i64::from(tm.tm_year) + 1900,
        month: i64::from(tm.tm_mon),
        day: i64::from(tm.tm_mday),
        weekday: i64::from(tm.tm_wday),
        hours: i64::from(tm.tm_hour),
        minutes: i64::from(tm.tm_min),
        seconds: i64::from(tm.tm_sec),
    })
}

/// `new Date(year, month, day, hours, minutes, seconds).getTime()`. 0~99년은 1900년대로 본다.
pub(crate) fn local_date_milliseconds(year: i64, month: i64, day: i64, hours: i64, minutes: i64, seconds: i64) -> f64 {
    let year = if (0..=99).contains(&year) { year + 1900 } else { year };
    // SAFETY: tm은 C 구조체라 0으로 채워도 유효하다.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    tm.tm_year = (year - 1900) as libc::c_int;
    tm.tm_mon = month as libc::c_int;
    tm.tm_mday = day as libc::c_int;
    tm.tm_hour = hours as libc::c_int;
    tm.tm_min = minutes as libc::c_int;
    tm.tm_sec = seconds as libc::c_int;
    tm.tm_isdst = -1;
    // SAFETY: 지역 tm 하나를 정규화하고 그 시각을 받는다.
    let time = unsafe { libc::mktime(&mut tm) };
    time as f64 * 1000.0
}

const MONTHS: [&str; 12] =
    ["January", "February", "March", "April", "May", "June", "July", "August", "September", "October", "November", "December"];
const WEEKDAYS: [&str; 7] = ["Sunday", "Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday"];

fn pad2(value: impl ToString) -> String {
    let text = value.to_string();
    if js_length(&text) < 2 { format!("0{text}") } else { text }
}

/// 리스 dateTimeFormat. 바꾼 글자를 다음 기호가 다시 바꾼다(April의 A가 AM/PM이 되는 것도 같다).
/// Invalid Date면 Intl 포맷이 던지므로 None이다.
fn date_time_format(format: &str, time: f64) -> Option<String> {
    let milliseconds = if time == 0.0 {
        now_milliseconds()
    } else if time.is_finite() && time.abs() <= 8.64e15 {
        time.trunc() + 0.0
    } else {
        f64::NAN
    };
    if format.is_empty() {
        return Some(String::new());
    }
    let format = format.strip_prefix(':').unwrap_or(format);
    if js_length(format) > 300 {
        return Some(String::new());
    }
    let date = local_time(milliseconds)?;
    let year = date.year.to_string();
    let day_of_year = ((milliseconds - local_date_milliseconds(date.year, 0, 0, 0, 0, 0)) / 86_400_000.0).floor();
    let month = MONTHS[date.month as usize];
    let weekday = WEEKDAYS[date.weekday as usize];
    let twelve = if date.hours % 12 == 0 { 12 } else { date.hours % 12 };
    Some(
        format
            .replace("YYYY", &year)
            .replace("YY", year.get(2..).unwrap_or(""))
            .replace("MMMM", month)
            .replace("MMM", &month[..3])
            .replace("MM", &pad2(date.month + 1))
            .replace("DDDD", &number_to_string(day_of_year))
            .replace("DD", &pad2(date.day))
            .replace("dddd", weekday)
            .replace("ddd", &weekday[..3])
            .replace("HH", &pad2(date.hours))
            .replace("hh", &pad2(twelve))
            .replace("mm", &pad2(date.minutes))
            .replace("ss", &pad2(date.seconds))
            .replace('X', &number_to_string((milliseconds / 1000.0).floor()))
            .replace('x', &number_to_string(milliseconds))
            .replace('A', if date.hours >= 12 { "PM" } else { "AM" }),
    )
}

fn current_time(args: &[&str], context: &CbsContext) -> Option<String> {
    let now = context.now.unwrap_or_else(now_milliseconds);
    if args.is_empty() {
        let date = local_time(now)?;
        return Some(format!("{}:{}:{}", date.hours, date.minutes, date.seconds));
    }
    let mut time = 0.0;
    if let Some(raw) = args.get(1).filter(|raw| !raw.is_empty()) {
        time = string_to_number(raw) / 1000.0;
        if time.is_nan() {
            time = 0.0;
        }
    }
    let time = if time != 0.0 { time } else { context.now.unwrap_or(0.0) };
    date_time_format(args[0], time)
}

/// 앱 메시지의 createdAt을 리스 메시지의 time처럼 쓴다.
fn idle_duration(context: &CbsContext) -> String {
    let Some(last) = context.messages.and_then(<[ChatMessage]>::last) else {
        return "00:00:00".to_string();
    };
    let Some(sent_at) = last.created_at.as_deref().and_then(date_parse) else {
        return "[Cannot get time, message was sent in older version]".to_string();
    };
    let now = context.now.unwrap_or_else(now_milliseconds);
    let mut seconds = ((now - sent_at) / 1000.0).floor();
    let mut minutes = (seconds / 60.0).floor();
    let hours = (minutes / 60.0).floor();
    seconds %= 60.0;
    minutes %= 60.0;
    format!("{}:{}:{}", number_to_string(hours), pad2(number_to_string(minutes)), pad2(number_to_string(seconds)))
}

/// `String.fromCharCode(parseInt(text, 16))`. 짝 없는 대리 문자는 Rust 문자열에 둘 수 없어 U+FFFD로 둔다.
fn char_from_hex(text: &str) -> String {
    let trimmed = text.trim_start_matches(is_js_whitespace);
    let (negative, digits) = match trimmed.as_bytes().first() {
        Some(b'-') => (true, &trimmed[1..]),
        Some(b'+') => (false, &trimmed[1..]),
        _ => (false, trimmed),
    };
    let digits = digits.strip_prefix("0x").or_else(|| digits.strip_prefix("0X")).unwrap_or(digits);
    let end = digits.bytes().position(|byte| !byte.is_ascii_hexdigit()).unwrap_or(digits.len());
    let significant = digits[..end].trim_start_matches('0');
    // 2^128 이상이면 double 값이 2^16의 배수(무한대 포함)라 ToUint16이 0이다. NaN도 0이다.
    let unit = if end == 0 || significant.len() > 32 {
        0
    } else {
        let exact = u128::from_str_radix(significant, 16).unwrap_or(0);
        // parseInt는 정수를 가장 가까운 double로 둔다.
        let rounded = exact as f64;
        let low = if rounded >= 2_f64.powi(128) { 0 } else { (rounded as u128 % 65_536) as u16 };
        if negative { 0_u16.wrapping_sub(low) } else { low }
    };
    char::from_u32(u32::from(unit)).unwrap_or('\u{fffd}').to_string()
}

const ESCAPES: [(char, char); 4] = [('{', '\u{e9b8}'), ('}', '\u{e9b9}'), ('(', '\u{e9ba}'), (')', '\u{e9bb}')];
const UNESCAPES: [&str; 8] = ["{", "}", "(", ")", "&lt;", "&gt;", ":", ";"];

fn risu_escape(text: &str) -> String {
    text.chars().map(|c| ESCAPES.iter().find(|(from, _)| *from == c).map_or(c, |(_, to)| *to)).collect()
}

/// 리스는 요청을 보내기 직전에 #escape로 숨긴 글자를 되돌린다.
pub fn risu_unescape(text: &str) -> String {
    let mut output = String::with_capacity(text.len());
    for c in text.chars() {
        match u32::from(c) {
            code @ 0xE9B8..=0xE9BF => output.push_str(UNESCAPES[(code - 0xE9B8) as usize]),
            _ => output.push(c),
        }
    }
    output
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum BlockKind {
    Parse,
    IfPure,
    Ignore,
    Escape,
    Each,
}

#[derive(Debug, Clone)]
struct Block {
    kind: BlockKind,
    keep: bool,
    each_source: String,
}

impl Block {
    /// 안쪽을 해석하지 않고 그대로 모아 두는 블록.
    fn is_pure(&self) -> bool {
        matches!(self.kind, BlockKind::Ignore | BlockKind::Each | BlockKind::Escape)
    }
}

fn block_start(body: &str) -> Option<Block> {
    let block = |kind, keep, each_source: &str| Some(Block { kind, keep, each_source: each_source.to_string() });
    if body.starts_with("#if") {
        let state = body.split(' ').nth(1);
        if matches!(state, Some("true" | "1")) {
            return block(if body.starts_with("#if_pure") { BlockKind::IfPure } else { BlockKind::Parse }, false, "");
        }
        return block(BlockKind::Ignore, false, "");
    }
    if body.starts_with("#escape") {
        return block(BlockKind::Escape, js_trim(&body[7..]) == "::keep", "");
    }
    if body.starts_with("#each") {
        let mut rest = js_trim(&body[5..]);
        let mut keep = false;
        if rest.starts_with("::keep ") {
            keep = true;
            rest = js_trim(&rest[7..]);
        }
        if rest.starts_with("as ") {
            rest = js_trim(&rest[3..]);
        }
        return block(BlockKind::Each, keep, rest);
    }
    None
}

fn trim_lines(text: &str) -> String {
    let joined = text.split('\n').map(|line| line.trim_start_matches(is_js_whitespace)).collect::<Vec<_>>().join("\n");
    js_trim(&joined).to_string()
}

fn block_end(text: &str, block: &Block) -> String {
    match block.kind {
        BlockKind::Parse => trim_lines(js_trim(text)),
        BlockKind::IfPure => text.to_string(),
        BlockKind::Each if block.keep => text.to_string(),
        BlockKind::Each => trim_lines(js_trim(text)),
        BlockKind::Escape => risu_escape(if block.keep { text } else { js_trim(text) }),
        BlockKind::Ignore => String::new(),
    }
}

/// #each 본문을 배열 항목마다 펼친다. 펼친 글은 원문에 다시 넣어 이어서 해석한다.
fn expand_each(body: &str, block: &Block) -> Option<String> {
    let source = block.each_source.as_str();
    let (name, array) = match source.rfind(" as ") {
        Some(index) => (js_trim(&source[index + 4..]), parse_array(Some(&source[..index]))),
        None => {
            let index = source.rfind(' ')?;
            (&source[index + 1..], parse_array(Some(&source[..index])))
        }
    };
    let search = format!("{{{{slot::{name}}}}}");
    let mut added = String::new();
    for item in array {
        let replacement = match item {
            Value::String(text) => text,
            other => json_text(&other),
        };
        added.push_str(&replace_all(body, &search, &replacement));
    }
    Some(if block.keep { added } else { js_trim(&added).to_string() })
}

enum Kind {
    Base,
    Function,
    Block(Block),
    PureChild,
}

struct Frame {
    text: String,
    kind: Kind,
}

struct Parser<'a> {
    context: CbsContext<'a>,
    /// 임시 변수(자기 속성). 값 None은 undefined를 넣은 것이다.
    temp_vars: Vec<(String, Option<String>)>,
    frames: Vec<Frame>,
}

impl Parser<'_> {
    fn top(&mut self) -> &mut Frame {
        let last = self.frames.len() - 1;
        &mut self.frames[last]
    }

    fn append(&mut self, text: &str) {
        self.top().text.push_str(text);
    }

    fn is_pure(&self) -> bool {
        self.frames.iter().any(|frame| matches!(&frame.kind, Kind::Block(block) if block.is_pure()))
    }

    fn push(&mut self, kind: Kind) {
        self.frames.push(Frame { text: String::new(), kind });
    }

    fn random(&self) -> f64 {
        self.context.random.map_or_else(math_random, |random| random())
    }

    fn temp_var(&self, key: &str) -> String {
        match self.temp_vars.iter().find(|(name, _)| name == key) {
            Some((_, value)) => value.clone().unwrap_or_default(),
            None => prototype_value(key).unwrap_or_default(),
        }
    }

    fn set_temp_var(&mut self, key: &str, value: Option<&str>) {
        // 객체에 __proto__로 문자열·undefined를 넣으면 아무 일도 없다.
        if key == "__proto__" {
            return;
        }
        let value = value.map(str::to_string);
        match self.temp_vars.iter_mut().find(|(name, _)| name == key) {
            Some(entry) => entry.1 = value,
            None => self.temp_vars.push((key.to_string(), value)),
        }
    }

    /// 리스처럼 함수 하나가 실패하면 None을 돌려 그 매크로만 글자 그대로 남긴다.
    fn call_function(&mut self, body: &str) -> Option<String> {
        if let Some(expression) = body.strip_prefix("? ") {
            return calc_string(expression, &self.context).map(number_to_string);
        }
        let parts: Vec<&str> = match body.find(':') {
            Some(index) if body[index + 1..].starts_with(':') => body.split("::").collect(),
            _ => body.split(':').collect(),
        };
        let name: String =
            parts[0].to_lowercase().chars().filter(|c| !(is_js_whitespace(*c) || *c == '_' || *c == '-')).collect();
        self.function(&name, &parts[1..])
    }

    fn function(&mut self, name: &str, args: &[&str]) -> Option<String> {
        let arg = |index: usize| args.get(index).copied();
        // 템플릿 문자열·String()에서 undefined는 "undefined"다.
        let text = |index: usize| arg(index).unwrap_or("undefined");
        let number = |index: usize| arg(index).map_or(f64::NAN, string_to_number);
        let flag = |value: bool| if value { "1" } else { "0" }.to_string();
        let context = self.context;
        Some(match name {
            "user" => context.user_name.unwrap_or("User").to_string(),
            "char" | "bot" => context.char_name.unwrap_or("Char").to_string(),
            "getglobalvar" => global_var(&context, text(0)),
            "length" => js_length(arg(0).unwrap_or("")).to_string(),
            "greaterequal" => flag(number(0) >= number(1)),
            "chatindex" => number_to_string(context.chat_index.unwrap_or(-1.0)),
            "lastmessageid" | "lastmessageindex" => number_to_string(context.last_message_index.unwrap_or(-1.0)),
            "random" => random_pick(args, self.random()),
            "equal" => flag(arg(0) == arg(1)),
            "notequal" => flag(arg(0) != arg(1)),
            "greater" => flag(number(0) > number(1)),
            "less" => flag(number(0) < number(1)),
            "and" => flag(arg(0) == Some("1") && arg(1) == Some("1")),
            "or" => flag(arg(0) == Some("1") || arg(1) == Some("1")),
            "not" => flag(arg(0) != Some("1")),
            "any" => flag(if args.len() > 1 {
                args.contains(&"1")
            } else {
                parse_array(arg(0)).iter().any(|item| item.as_str() == Some("1"))
            }),
            "contains" => flag(arg(0)?.contains(text(1))),
            "replace" => replace_all(arg(0)?, text(1), text(2)),
            "trim" => js_trim(arg(0)?).to_string(),
            "blank" | "none" => String::new(),
            "u" | "unicodedecodefromhex" => char_from_hex(text(0)),
            "ruby" | "furigana" => format!("<ruby>{}<rp> (</rp><rt>{}</rt><rp>) </rp></ruby>", text(0), text(1)),
            "button" => format!("<button class=\"button-default\" risu-trigger=\"{}\">{}</button>", text(1), text(0)),
            "makearray" | "array" | "a" => {
                json_text(&args.iter().map(|item| item.replace("::", "\\u003A\\u003A")).collect::<Vec<_>>())
            }
            "tempvar" | "gettempvar" => self.temp_var(text(0)),
            "settempvar" => {
                self.set_temp_var(text(0), arg(1));
                String::new()
            }
            "roll" => match context.random {
                Some(random) => roll_dice(args, random),
                None => roll_dice(args, &math_random),
            },
            "time" => current_time(args, &context)?,
            "idleduration" => idle_duration(&context),
            "previouschatlog" => {
                let index = number(0);
                let found = (is_js_integer(index) && index >= 0.0)
                    .then(|| context.messages.and_then(|messages| messages.get(index as usize)))
                    .flatten();
                found.map_or_else(|| "Out of range".to_string(), |message| message.content.clone())
            }
            _ => return None,
        })
    }

    /// `}}`로 닫힌 본문 처리. #each를 펼친 글이 있으면 돌려주어 원문 현재 위치 뒤에 넣는다.
    fn handle_close(&mut self, body: String) -> Option<String> {
        let literal = format!("{{{{{body}}}}}");
        if body.starts_with('#') || body.starts_with(':') {
            if self.is_pure() {
                self.append(&literal);
                if body != ":else" {
                    self.push(Kind::PureChild);
                }
                return None;
            }
            match block_start(&body) {
                None => self.append(&literal),
                Some(block) => self.push(Kind::Block(block)),
            }
            return None;
        }
        if body.starts_with('/') && !body.starts_with("//") {
            match self.frames.last().map(|frame| &frame.kind) {
                Some(Kind::Block(_)) => {
                    let Some(Frame { text, kind: Kind::Block(block) }) = self.frames.pop() else {
                        return None;
                    };
                    let result = block_end(&text, &block);
                    if block.kind == BlockKind::Each {
                        return expand_each(&result, &block);
                    }
                    if !result.is_empty() {
                        self.append(&result);
                    }
                    return None;
                }
                Some(Kind::PureChild) => {
                    let inner = self.frames.pop().map(|frame| frame.text).unwrap_or_default();
                    self.append(&format!("{inner}{literal}"));
                    return None;
                }
                _ => {}
            }
        }
        let result = if self.is_pure() { None } else { self.call_function(&body) };
        self.append(result.as_deref().unwrap_or(&literal));
        None
    }

    fn finish(mut self) -> String {
        let mut rest = String::new();
        while self.frames.len() > 1 {
            let Some(frame) = self.frames.pop() else { break };
            let opener = if matches!(frame.kind, Kind::Function) { "{{" } else { "<" };
            rest = format!("{opener}{}{rest}", frame.text);
        }
        let base = self.frames.pop().map(|frame| frame.text).unwrap_or_default();
        base + &rest
    }
}

/// `<user>`·`<char>`·`<bot>`(ASCII 대소문자 무시)을 `{{…}}`로 바꾼다. 글자의 원래 대소문자는 남는다.
fn replace_name_tags(source: &str) -> String {
    let mut output = String::with_capacity(source.len());
    let mut rest = source;
    while let Some(index) = rest.find('<') {
        output.push_str(&rest[..index]);
        let after = &rest[index + 1..];
        let bytes = after.as_bytes();
        let length = ["user", "char", "bot"]
            .iter()
            .find(|name| bytes.len() > name.len() && bytes[..name.len()].eq_ignore_ascii_case(name.as_bytes()) && bytes[name.len()] == b'>')
            .map(|name| name.len());
        match length {
            Some(length) => {
                output.push_str("{{");
                output.push_str(&after[..length]);
                output.push_str("}}");
                rest = &after[length + 1..];
            }
            None => {
                output.push('<');
                rest = after;
            }
        }
    }
    output.push_str(rest);
    output
}

/// 리스 parseCBS. 임시 변수는 해석 한 번 안에서만 산다.
pub fn parse_cbs(source: &str, context: &CbsContext) -> String {
    let mut text = replace_name_tags(source);
    let mut parser = Parser {
        context: *context,
        temp_vars: Vec::new(),
        frames: vec![Frame { text: String::new(), kind: Kind::Base }],
    };
    let mut pointer = 0;
    while pointer < text.len() {
        let rest = &text.as_bytes()[pointer..];
        if rest.starts_with(b"{{") {
            pointer += 2;
            parser.push(Kind::Function);
        } else if rest.starts_with(b"}}") && parser.frames.len() > 1 && matches!(parser.frames.last().map(|frame| &frame.kind), Some(Kind::Function)) {
            pointer += 2;
            let body = parser.frames.pop().map(|frame| frame.text).unwrap_or_default();
            if let Some(added) = parser.handle_close(body) {
                text.insert_str(pointer, &added);
            }
        } else {
            let c = text[pointer..].chars().next().unwrap_or_default();
            parser.top().text.push(c);
            pointer += c.len_utf8();
        }
    }
    parser.finish()
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;
    use crate::js::iso_string;

    fn vars(entries: &[(&str, &str)]) -> HashMap<String, String> {
        entries.iter().map(|(key, value)| (key.to_string(), value.to_string())).collect()
    }

    fn parse(source: &str) -> String {
        parse_cbs(source, &CbsContext::default())
    }

    #[test]
    fn cbs_조건_계산_이름을_리스_규칙대로_푼다() {
        let on = vars(&[("toggle_on", "1")]);
        let empty = HashMap::new();
        let context = CbsContext { user_name: Some("민지"), char_name: Some("코대리"), vars: Some(&on), ..Default::default() };
        let condition = "{{#if_pure {{? {{getglobalvar::toggle_on}}=1}}}}켜짐{{/if}}";
        assert_eq!(parse_cbs(condition, &context), "켜짐");
        assert_eq!(parse_cbs(condition, &CbsContext { vars: Some(&empty), ..Default::default() }), "");
        assert_eq!(parse("{{? {{? 1>0}} || {{? 0>1}}=1}}"), "1");
        assert_eq!(parse_cbs("{{? {{getglobalvar::toggle_none}}>0}}", &CbsContext { vars: Some(&empty), ..Default::default() }), "0");
        assert_eq!(parse("{{? 1+2*3}}"), "7");
        assert_eq!(parse_cbs("{{user}}와 <char>", &context), "민지와 코대리");
        assert_eq!(parse("{{#if 1}}\n   첫줄\n   둘째{{/if}}"), "첫줄\n둘째");
        assert_eq!(parse("{{#if_pure 1}}\n  그대로{{/if}}"), "\n  그대로");
        assert_eq!(parse("{{#if 0}}{{#if 1}}안{{/if}}밖{{/if}}뒤"), "뒤");
        assert_eq!(parse("{{unknownfn::x}} {{abc"), "{{unknownfn::x}} {{abc");
    }

    #[test]
    fn 소악마가_쓰는_cbs_비교_문자열_배열_함수를_리스_규칙대로_푼다() {
        assert_eq!(parse("{{equal::a::a}}{{equal::a::b}}{{not_equal::a::b}}{{notequal::a::a}}"), "1010");
        assert_eq!(parse("{{greater::3::2}}{{less::3::2}}{{greater_equal::2::2}}"), "101");
        assert_eq!(parse("{{and::1::1}}{{and::1::0}}{{or::0::1}}{{or::0::0}}{{not::1}}{{not::0}}"), "101001");
        assert_eq!(parse("{{any::0::1}}{{any::0::0}}{{any::{{array::0::1}}}}"), "101");
        assert_eq!(parse("{{contains::헬레나가 웃었다::헬레나}}{{contains::abc::d}}"), "10");
        assert_eq!(parse("[{{trim::  가 }}][{{replace::a-b-c::-::+}}][{{blank}}][{{u::41}}]"), "[가][a+b+c][][A]");
        assert_eq!(parse("{{ruby::漢::한}}"), "<ruby>漢<rp> (</rp><rt>한</rt><rp>) </rp></ruby>");
        assert_eq!(parse("{{array::a::b}}"), "[\"a\",\"b\"]");
        let half = || 0.5;
        assert_eq!(parse_cbs("{{roll::2d6}}", &CbsContext { random: Some(&half), ..Default::default() }), "8");
        assert_eq!(parse("{{roll::0d6}}"), "NaN");
        // 리스처럼 함수가 실패하면 그 매크로를 글자 그대로 남긴다.
        assert_eq!(parse("{{contains}}"), "{{contains}}");
        assert_eq!(parse("{{? 가 || 나}}"), "{{? 가 || 나}}");
        assert_eq!(parse("{{? {{nofn::x}} || 1}}"), "1");
    }

    #[test]
    fn cbs_임시_변수_반복문_escape를_리스_규칙대로_푼다() {
        assert_eq!(parse("{{settempvar::a::켜짐}}[{{tempvar::a}}][{{gettempvar::a}}][{{tempvar::b}}]"), "[켜짐][켜짐][]");
        assert_eq!(parse("{{tempvar::a}}"), "");
        // 리스 loop.test.ts의 기대값.
        assert_eq!(parse("{{#each [1, 2, 3] as n}}{{slot::n}} {{/}}"), "123");
        assert_eq!(parse("{{#each::keep [[1, 2], [3, 4]] as x}}{{#each::keep {{slot::x}} as y}}{{slot::y}}\n{{/}}{{/}}"), "1\n2\n3\n4\n");
        assert_eq!(parse("{{#each {{array::x::y}} v}}[{{slot::v}}]{{/each}}"), "[x][y]");
        let genre_vars = vars(&[("toggle_g1", "2"), ("toggle_g2", "0")]);
        let genre = "{{settempvar::hit::0}}{{#each {{array::toggle_g1::toggle_g2}} g}}{{#if {{equal::{{getglobalvar::{{slot::g}}}}::2}}}}{{settempvar::hit::1}}{{/if}}{{/each}}{{tempvar::hit}}";
        assert_eq!(parse_cbs(genre, &CbsContext { vars: Some(&genre_vars), ..Default::default() }), "1");
        assert_eq!(parse("{{#escape}} {bkspc}(x) {{/}}"), "\u{e9b8}bkspc\u{e9b9}\u{e9ba}x\u{e9bb}");
        let context = CbsContext { user_name: Some("민지"), ..Default::default() };
        assert_eq!(risu_unescape(&parse_cbs("{{#escape}}{{user}}{{/escape}}", &context)), "{{user}}");
    }

    fn message(id: &str, content: &str, created_at: Option<f64>) -> ChatMessage {
        ChatMessage { id: id.into(), role: "user".into(), content: content.into(), created_at: created_at.map(iso_string) }
    }

    #[test]
    fn cbs_대화_기록_시각_함수는_앱_메시지를_리스_메시지처럼_읽는다() {
        let now = local_date_milliseconds(2026, 9, 25, 9, 5, 7);
        let messages = [message("1", "안녕", Some(now - 3_725_000.0)), message("2", "헬레나 등장", Some(now - 65_000.0))];
        let context = CbsContext { messages: Some(&messages), now: Some(now), ..Default::default() };
        assert_eq!(parse_cbs("{{previous_chat_log::1}}|{{previous_chat_log::5}}", &context), "헬레나 등장|Out of range");
        assert_eq!(parse_cbs("{{idle_duration}}", &context), "0:01:05");
        assert_eq!(parse_cbs("{{idle_duration}}", &CbsContext { messages: Some(&[]), ..Default::default() }), "00:00:00");
        assert_eq!(parse_cbs("{{time::YYYY-MM-DD HH:mm}}", &context), "2026-10-25 09:05");
        assert_eq!(parse_cbs("{{time}}", &context), "9:5:7");
        let loop_source = "{{#each {{? {{lastmessageid}}}} item}}{{#if {{contains::{{previous_chat_log::{{slot::item}}}}::헬레나}}}}찾음{{/if}}{{/each}}";
        assert_eq!(parse_cbs(loop_source, &CbsContext { last_message_index: Some(1.0), ..context }), "찾음");
        let old = [ChatMessage { created_at: None, ..message("1", "옛", None) }];
        assert_eq!(
            parse_cbs("{{idle_duration}}", &CbsContext { messages: Some(&old), ..Default::default() }),
            "[Cannot get time, message was sent in older version]"
        );
    }

    /// /tmp/office-rs-compare/r17-cbs-cases.mjs로 Node 원본에서 뽑은 값.
    #[test]
    fn cbs_경계_동작은_node_원본과_같다() {
        let now = local_date_milliseconds(2026, 3, 1, 21, 4, 9);
        let toggles = vars(&[("toggle_a", "2.5"), ("toggle_b", "x")]);
        let half = || 0.5;
        let messages = [message("1", "첫", Some(now + 5000.0))];
        let context =
            CbsContext { now: Some(now), vars: Some(&toggles), random: Some(&half), messages: Some(&messages), ..Default::default() };
        let cases: &[(&str, &str)] = &[
            ("{{? 1/0}}", "Infinity"),
            ("{{? 0.0}}", "0"),
            ("{{? !1}}", "0"),
            ("{{? 2^3^2}}", "64"),
            ("{{? 5%3}}", "2"),
            ("{{? 1=1&&2=2}}", "1"),
            ("{{? -3+5}}", "2"),
            ("{{? null+1}}", "1"),
            ("{{? @toggle_a*2}}", "5"),
            ("{{? @toggle_b+1}}", "1"),
            ("{{? $x+1}}", "1"),
            ("{{? (1+2)*3}}", "9"),
            ("{{? 1+(2}}", "3"),
            ("{{? 3)}}", "3"),
            ("{{? 1 || 0}}", "1"),
            ("{{? 0 && 5}}", "0"),
            ("{{? 1^(0/0)}}", "NaN"),
            ("{{? 0.1+0.2}}", "0.30000000000000004"),
            ("{{? 10>=10}}", "1"),
            ("{{? 3!=4}}", "1"),
            ("{{? 1e21*1}}", "1e+21"),
            ("{{u::0x41}}", "A"),
            ("{{u::-1}}", "\u{ffff}"),
            ("{{u::zz}}", "\u{0}"),
            ("{{u::1F600}}", "\u{f600}"),
            ("{{u}}", "\u{0}"),
            ("{{time::MMMM ddd A}}", "PMpril Wed PM"),
            ("{{time:::dddd}}", "Wednesday"),
            ("{{time::}}", ""),
            ("{{time::HH::1e20}}", "{{time::HH::1e20}}"),
            ("{{time::YYYY::86400000000}}", "1970"),
            ("{{random::a,b\\,c,d}}", "b,c"),
            ("{{random::a::b::c}}", "b"),
            ("{{random}}", "0.5"),
            ("{{roll::d}}", "4"),
            ("{{roll::3}}", "2"),
            ("{{roll::1d1d1}}", "4"),
            ("{{roll::2.5d2}}", "6"),
            ("{{roll}}", "1"),
            ("{{getglobalvar::constructor}}", "function Object() { [native code] }"),
            ("{{getglobalvar::__proto__}}", "[object Object]"),
            ("{{tempvar::hasOwnProperty}}", "function hasOwnProperty() { [native code] }"),
            ("{{settempvar::__proto__::x}}{{tempvar::__proto__}}", "[object Object]"),
            ("{{length::😀a}}", "3"),
            ("{{contains::abc}}", "0"),
            ("{{replace::a$b::$::[$&]}}", "a[$]b"),
            ("{{replace::abc::b}}", "aundefinedc"),
            ("{{ruby::a}}", "<ruby>a<rp> (</rp><rt>undefined</rt><rp>) </rp></ruby>"),
            ("{{button::x}}", "<button class=\"button-default\" risu-trigger=\"undefined\">x</button>"),
            ("{{#each [1, {\"a\":2}, null, \"s\"] as v}}<{{slot::v}}>{{/each}}", "<1><{\"a\":2}><null><s>"),
            ("{{#each a§b§c x}}{{slot::x}},{{/each}}", "a,b,c,"),
            ("{{#each nospace}}x{{/each}}", ""),
            ("{{#each [\"$&\"] as v}}[{{slot::v}}]{{/each}}", "[{{slot::v}}]"),
            ("{{#if true}}t{{:else}}e{{/if}}", "t{{:else}}e"),
            ("{{#if_pure 0}}x{{/if}}y", "y"),
            ("{{#iffy 1}}z{{/}}", "z"),
            ("{{#escape::keep}} {a} {{/}}", " \u{e9b8}a\u{e9b9} "),
            ("{{#escape}}{{#if 1}}x{{/if}}{{/escape}}", "\u{e9b8}\u{e9b8}#if 1\u{e9b9}\u{e9b9}x\u{e9b8}\u{e9b8}/if\u{e9b9}\u{e9b9}"),
            ("{{#unknown}}a{{/x}}", "{{#unknown}}a{{/x}}"),
            ("{{:else}}", "{{:else}}"),
            ("{{//c}}", "{{//c}}"),
            ("{{/}}", "{{/}}"),
            ("<USER> <Char> <bot> <usr>", "User Char Char <usr>"),
            ("{{#if 1}}a{{b", "<a{{b"),
            ("{{#each [1] as n}}{{slot::n}}", "<{{slot::n}}"),
            ("{{a{{b}}c", "{{a{{b}}c"),
            ("}}{{", "}}{{"),
            (
                "{{chatindex}}|{{chat_index}}|{{lastmessageindex}}|{{greater_equal::1::2}}|{{none}}|{{furigana::a::b}}|{{a::x}}|{{gettempvar::q}}",
                "-1|-1|-1|0||<ruby>a<rp> (</rp><rt>b</rt><rp>) </rp></ruby>|[\"x\"]|",
            ),
            ("{{ U s-E_r }}", "User"),
            ("{{previous_chat_log::0}}|{{previous_chat_log}}|{{previouschatlog::-0}}", "첫|Out of range|첫"),
            ("{{idle_duration}}", "-1:-1:-5"),
            ("{{any}}{{any::1}}{{any::[1,\"1\"]}}{{equal}}{{notequal::a}}{{and::1}}{{not}}", "0111101"),
        ];
        for (source, expected) in cases {
            assert_eq!(parse_cbs(source, &context), *expected, "{source}");
        }
        // Node(서울 시간대)에서는 "26/91 09 1775045049 1775045049000"이다. 기준 시각만 시간대를 따른다.
        let epoch = (now / 1000.0).floor();
        assert_eq!(parse_cbs("{{time::YY/DDDD hh X x}}", &context), format!("26/91 09 {epoch} {now}"));
        assert_eq!(risu_unescape("\u{e9b8}\u{e9b9}\u{e9ba}\u{e9bb}\u{e9bc}\u{e9bd}\u{e9be}\u{e9bf}"), "{}()&lt;&gt;:;");
    }
}
