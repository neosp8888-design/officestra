// JS(Node) 동작을 그대로 따라야 하는 문자열·숫자·날짜·경로 규칙. 응답 바이트를 Node와 맞추는 데 쓴다.

use std::cmp::Ordering;
use std::io;

use serde::Serialize;
use serde_json::Value;
use serde_json::ser::{Formatter, Serializer};

/// JS String.prototype.trim과 StringToNumber가 지우는 공백(WhiteSpace와 LineTerminator). 정규식 `\s`와 같다.
pub fn is_js_whitespace(c: char) -> bool {
    matches!(
        c,
        '\u{9}'..='\u{d}' | ' ' | '\u{a0}' | '\u{1680}' | '\u{2000}'..='\u{200a}'
            | '\u{2028}' | '\u{2029}' | '\u{202f}' | '\u{205f}' | '\u{3000}' | '\u{feff}'
    )
}

pub fn js_trim(value: &str) -> &str {
    value.trim_matches(is_js_whitespace)
}

/// JS 문자열의 length(UTF-16 단위 수).
pub fn js_length(value: &str) -> usize {
    value.encode_utf16().count()
}

/// Array.prototype.sort의 기본 비교. UTF-16 단위 순서다.
pub fn js_compare(left: &str, right: &str) -> Ordering {
    left.encode_utf16().cmp(right.encode_utf16())
}

/// `Math.min(a, b)`. NaN이 하나라도 있으면 NaN이다.
pub fn js_min(left: f64, right: f64) -> f64 {
    if left.is_nan() || right.is_nan() { f64::NAN } else { left.min(right) }
}

/// `Math.max(a, b)`. NaN이 하나라도 있으면 NaN이고 -0보다 +0이 크다.
pub fn js_max(left: f64, right: f64) -> f64 {
    if left.is_nan() || right.is_nan() {
        f64::NAN
    } else if left == right {
        if left.is_sign_positive() { left } else { right }
    } else {
        left.max(right)
    }
}

/// `Number.isInteger(value)`.
pub fn is_js_integer(value: f64) -> bool {
    value.is_finite() && value.trunc() == value
}

/// JS `Number(text)`(StringToNumber).
pub fn string_to_number(text: &str) -> f64 {
    let text = js_trim(text);
    if text.is_empty() {
        return 0.0;
    }
    let bytes = text.as_bytes();
    if bytes.len() > 2 && bytes[0] == b'0' {
        let radix = match bytes[1] {
            b'x' | b'X' => 16,
            b'o' | b'O' => 8,
            b'b' | b'B' => 2,
            _ => 0,
        };
        if radix != 0 {
            let digits = &text[2..];
            if !digits.chars().all(|c| c.is_digit(radix)) {
                return f64::NAN;
            }
            return digits.chars().fold(0.0, |total, c| {
                total * f64::from(radix) + f64::from(c.to_digit(radix).unwrap_or(0))
            });
        }
    }
    let (sign, unsigned) = match bytes[0] {
        b'+' => (1.0, &text[1..]),
        b'-' => (-1.0, &text[1..]),
        _ => (1.0, text),
    };
    if unsigned == "Infinity" {
        return sign * f64::INFINITY;
    }
    if !is_decimal_literal(unsigned) {
        return f64::NAN;
    }
    unsigned.parse::<f64>().map_or(f64::NAN, |value| sign * value)
}

/// StrUnsignedDecimalLiteral: 숫자열과 소수점, 지수. Rust parse가 받는 inf·nan 같은 꼴을 미리 거른다.
fn is_decimal_literal(text: &str) -> bool {
    let bytes = text.as_bytes();
    let digits_from = |start: usize| bytes[start..].iter().take_while(|byte| byte.is_ascii_digit()).count();
    let mut index = 0;
    let integer_digits = digits_from(index);
    index += integer_digits;
    let mut fraction_digits = 0;
    if bytes.get(index) == Some(&b'.') {
        index += 1;
        fraction_digits = digits_from(index);
        index += fraction_digits;
    }
    if integer_digits == 0 && fraction_digits == 0 {
        return false;
    }
    if matches!(bytes.get(index), Some(b'e' | b'E')) {
        index += 1;
        if matches!(bytes.get(index), Some(b'+' | b'-')) {
            index += 1;
        }
        let exponent_digits = digits_from(index);
        if exponent_digits == 0 {
            return false;
        }
        index += exponent_digits;
    }
    index == bytes.len()
}

/// JS `String(number)`(Number::toString). 가장 짧은 왕복 자릿수에 JS의 지수 표기 규칙을 쓴다.
pub fn number_to_string(value: f64) -> String {
    if value.is_nan() {
        return "NaN".into();
    }
    if value == 0.0 {
        return "0".into();
    }
    if value.is_infinite() {
        return if value > 0.0 { "Infinity".into() } else { "-Infinity".into() };
    }
    if value < 0.0 {
        return format!("-{}", number_to_string(-value));
    }
    let scientific = format!("{value:e}");
    let (mantissa, exponent) = scientific.split_once('e').unwrap_or((&scientific, "0"));
    let exponent: i32 = exponent.parse().unwrap_or(0);
    let digits: String = mantissa.chars().filter(|c| *c != '.').collect();
    let k = digits.len() as i32;
    let n = exponent + 1;
    if k <= n && n <= 21 {
        format!("{digits}{}", "0".repeat((n - k) as usize))
    } else if 0 < n && n <= 21 {
        format!("{}.{}", &digits[..n as usize], &digits[n as usize..])
    } else if -6 < n && n <= 0 {
        format!("0.{}{digits}", "0".repeat((-n) as usize))
    } else {
        let power = n - 1;
        let sign = if power >= 0 { '+' } else { '-' };
        if k == 1 {
            format!("{digits}e{sign}{}", power.abs())
        } else {
            format!("{}.{}e{sign}{}", &digits[..1], &digits[1..], power.abs())
        }
    }
}

/// JSON.stringify와 같은 숫자 표기. serde_json 기본(ryu)은 0.00005를 5e-5, 1.0을 1.0으로 쓴다.
pub struct JsFormatter;

impl Formatter for JsFormatter {
    fn write_f64<W: ?Sized + io::Write>(&mut self, writer: &mut W, value: f64) -> io::Result<()> {
        if value.is_finite() {
            writer.write_all(number_to_string(value).as_bytes())
        } else {
            writer.write_all(b"null")
        }
    }

    fn write_f32<W: ?Sized + io::Write>(&mut self, writer: &mut W, value: f32) -> io::Result<()> {
        self.write_f64(writer, f64::from(value))
    }

    // JS 숫자는 double이라 2^53을 넘는 정수(예: json 열 안의 큰 수)는 가장 가까운 double로 쓴다.
    fn write_i64<W: ?Sized + io::Write>(&mut self, writer: &mut W, value: i64) -> io::Result<()> {
        if value.unsigned_abs() <= MAX_SAFE_INTEGER {
            writer.write_all(value.to_string().as_bytes())
        } else {
            self.write_f64(writer, value as f64)
        }
    }

    fn write_u64<W: ?Sized + io::Write>(&mut self, writer: &mut W, value: u64) -> io::Result<()> {
        if value <= MAX_SAFE_INTEGER {
            writer.write_all(value.to_string().as_bytes())
        } else {
            self.write_f64(writer, value as f64)
        }
    }
}

const MAX_SAFE_INTEGER: u64 = (1 << 53) - 1;

/// JSON.stringify(value)와 같은 바이트.
pub fn to_js_json(value: &impl Serialize) -> serde_json::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    let mut serializer = Serializer::with_formatter(&mut bytes, JsFormatter);
    value.serialize(&mut serializer)?;
    Ok(bytes)
}

/// `JSON.stringify(value, null, 2)`. 들여쓰기는 두 칸이고 숫자 표기는 to_js_json과 같다.
pub fn to_js_json_pretty(value: &Value) -> String {
    fn write(value: &Value, depth: usize, output: &mut String) {
        let indent = |depth: usize| "  ".repeat(depth);
        match value {
            Value::Array(items) if !items.is_empty() => {
                output.push('[');
                for (index, item) in items.iter().enumerate() {
                    output.push_str(if index == 0 { "\n" } else { ",\n" });
                    output.push_str(&indent(depth + 1));
                    write(item, depth + 1, output);
                }
                output.push('\n');
                output.push_str(&indent(depth));
                output.push(']');
            }
            Value::Object(fields) if !fields.is_empty() => {
                output.push('{');
                for (index, (key, item)) in fields.iter().enumerate() {
                    output.push_str(if index == 0 { "\n" } else { ",\n" });
                    output.push_str(&indent(depth + 1));
                    output.push_str(&String::from_utf8(to_js_json(key).unwrap_or_default()).unwrap_or_default());
                    output.push_str(": ");
                    write(item, depth + 1, output);
                }
                output.push('\n');
                output.push_str(&indent(depth));
                output.push('}');
            }
            leaf => output.push_str(&String::from_utf8(to_js_json(leaf).unwrap_or_default()).unwrap_or_default()),
        }
    }
    let mut output = String::new();
    write(value, 0, &mut output);
    output
}

/// JS `String(value)`. 배열은 join(","), 객체는 "[object Object]"다.
pub fn js_string(value: &Value) -> String {
    match value {
        Value::Null => "null".into(),
        Value::Bool(flag) => flag.to_string(),
        Value::Number(number) => number_to_string(number.as_f64().unwrap_or(f64::NAN)),
        Value::String(text) => text.clone(),
        Value::Array(items) => js_join(items, ","),
        Value::Object(_) => "[object Object]".into(),
    }
}

/// JS `String(value ?? "")`. 없는 값과 null은 빈 문자열이다.
pub fn js_string_or_empty(value: Option<&Value>) -> String {
    match value {
        None | Some(Value::Null) => String::new(),
        Some(value) => js_string(value),
    }
}

/// `Array.prototype.join(separator)`. null 요소는 빈 문자열이다.
pub fn js_join(items: &[Value], separator: &str) -> String {
    items
        .iter()
        .map(|item| if item.is_null() { String::new() } else { js_string(item) })
        .collect::<Vec<_>>()
        .join(separator)
}

/// JS `Number(value)`. 없는 값(undefined)은 호출하는 쪽에서 NaN으로 다룬다.
pub fn js_to_number(value: &Value) -> f64 {
    match value {
        Value::Null => 0.0,
        Value::Bool(flag) => f64::from(u8::from(*flag)),
        Value::Number(number) => number.as_f64().unwrap_or(f64::NAN),
        Value::String(text) => string_to_number(text),
        Value::Array(_) => string_to_number(&js_string(value)),
        Value::Object(_) => f64::NAN,
    }
}

/// JS 참거짓 판정. 없는 값(undefined)은 거짓이다.
pub fn js_truthy(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) => false,
        Some(Value::Bool(flag)) => *flag,
        Some(Value::Number(number)) => number.as_f64().is_some_and(|number| number != 0.0),
        Some(Value::String(text)) => !text.is_empty(),
        Some(_) => true,
    }
}

/// JS `body.key`. body가 null이면 Node와 같은 TypeError 문구를 낸다. 객체가 아니면 undefined다.
pub fn js_property<'a>(body: &'a Value, key: &str) -> Result<Option<&'a Value>, String> {
    match body {
        Value::Null => Err(format!("Cannot read properties of null (reading '{key}')")),
        Value::Object(fields) => Ok(fields.get(key)),
        _ => Ok(None),
    }
}

/// node-pg prepareValue가 보내는 매개변수 텍스트. null은 SQL NULL, 배열은 PG 배열 문자열, 객체는 JSON이다.
pub fn pg_text(value: &Value) -> Option<String> {
    match value {
        Value::Null => None,
        Value::String(text) => Some(text.clone()),
        Value::Array(items) => Some(pg_array(items)),
        Value::Object(_) => Some(String::from_utf8(to_js_json(value).unwrap_or_default()).unwrap_or_default()),
        other => Some(js_string(other)),
    }
}

fn pg_array(items: &[Value]) -> String {
    let mut text = String::from("{");
    for (index, item) in items.iter().enumerate() {
        if index > 0 {
            text.push(',');
        }
        match item {
            Value::Null => text.push_str("NULL"),
            Value::Array(inner) => text.push_str(&pg_array(inner)),
            other => {
                let element = pg_text(other).unwrap_or_default();
                text.push('"');
                text.push_str(&element.replace('\\', "\\\\").replace('"', "\\\""));
                text.push('"');
            }
        }
    }
    text.push('}');
    text
}

/// JSON.parse. 짝 없는 대리 문자 이스케이프(`\ud83d` 등)는 serde_json이 거부하므로 U+FFFD로 바꿔 다시 읽는다.
/// Node CLI의 JSON.stringify는 잘린 이모지를 이렇게 내보내며, Node 백엔드도 그 글자를 node-pg로 저장할 때 U+FFFD가 된다.
pub fn parse_js_json(text: &str) -> serde_json::Result<Value> {
    match serde_json::from_str(text) {
        Err(error) => match replace_lone_surrogate_escapes(text) {
            Some(replaced) => serde_json::from_str(&replaced),
            None => Err(error),
        },
        parsed => parsed,
    }
}

/// 문자열 안의 짝 없는 `\uD800`~`\uDFFF` 이스케이프를 `�`로 바꾼다. 바꿀 것이 없으면 None이다.
fn replace_lone_surrogate_escapes(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let unit_at = |index: usize| -> Option<u16> {
        (bytes.get(index) == Some(&b'\\') && bytes.get(index + 1) == Some(&b'u'))
            .then(|| std::str::from_utf8(bytes.get(index + 2..index + 6)?).ok())
            .flatten()
            .and_then(|hex| u16::from_str_radix(hex, 16).ok())
    };
    let mut output = String::new();
    let mut copied = 0;
    let mut in_string = false;
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'"' => in_string = !in_string,
            b'\\' if in_string => {
                let Some(unit) = unit_at(index) else {
                    index += 2;
                    continue;
                };
                let paired = (0xD800..0xDC00).contains(&unit) && unit_at(index + 6).is_some_and(|low| (0xDC00..0xE000).contains(&low));
                if paired {
                    index += 12;
                    continue;
                }
                if (0xD800..0xE000).contains(&unit) {
                    output.push_str(&text[copied..index]);
                    output.push_str("\\uFFFD");
                    copied = index + 6;
                }
                index += 6;
                continue;
            }
            _ => {}
        }
        index += 1;
    }
    (copied > 0).then(|| output + &text[copied..])
}

/// JS decodeURIComponent. 잘못된 % 순서나 UTF-8이 아니면 None(Node는 URIError "URI malformed")이다.
pub fn decode_uri_component(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] != b'%' {
            decoded.push(bytes[index]);
            index += 1;
            continue;
        }
        let hex = bytes.get(index + 1..index + 3)?;
        if !hex.iter().all(u8::is_ascii_hexdigit) {
            return None;
        }
        decoded.push(u8::from_str_radix(std::str::from_utf8(hex).ok()?, 16).ok()?);
        index += 3;
    }
    String::from_utf8(decoded).ok()
}

/// JS encodeURIComponent. 영문·숫자와 `-_.!~*'()`만 두고 나머지는 UTF-8 바이트마다 대문자 %XX로 바꾼다.
/// Rust 문자열에는 짝 없는 대리 코드가 없으므로 URIError가 날 일이 없다.
pub fn encode_uri_component(text: &str) -> String {
    let mut encoded = String::with_capacity(text.len());
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || b"-_.!~*'()".contains(&byte) {
            encoded.push(char::from(byte));
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    encoded
}

/// URLSearchParams. `+`는 공백, 잘못된 %는 그대로, UTF-8이 아니면 대체 문자로 읽는다.
pub struct SearchParams(Vec<(String, String)>);

impl SearchParams {
    pub fn parse(query: Option<&str>) -> Self {
        let query = query.unwrap_or("");
        Self(url::form_urlencoded::parse(query.as_bytes()).into_owned().collect())
    }

    /// `searchParams.get(name)`. 같은 이름이 여럿이면 처음 값이다.
    pub fn get(&self, name: &str) -> Option<&str> {
        self.0.iter().find(|(key, _)| key == name).map(|(_, value)| value.as_str())
    }
}

fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year - era * 400;
    let month_index = (month + 9) % 12;
    let day_of_year = (153 * month_index + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

/// `Date.parse(text)`의 ISO 꼴(YYYY[-MM[-DD]][THH:mm[:ss[.s+]]][Z|±HH:mm]). 날짜만이면 UTC다.
/// 시각이 있는데 시간대가 없으면 JS는 실행 지역 시간으로 읽으므로 여기서는 None으로 둔다.
pub fn date_parse(text: &str) -> Option<f64> {
    let bytes = text.as_bytes();
    let mut index = 0;
    let number = |index: &mut usize, width: usize| -> Option<i64> {
        let digits = bytes.get(*index..*index + width)?;
        if !digits.iter().all(u8::is_ascii_digit) {
            return None;
        }
        *index += width;
        std::str::from_utf8(digits).ok()?.parse().ok()
    };
    let year = match bytes.first()? {
        b'+' | b'-' => {
            let negative = bytes[0] == b'-';
            index += 1;
            let value = number(&mut index, 6)?;
            if negative && value == 0 {
                return None;
            }
            if negative { -value } else { value }
        }
        _ => number(&mut index, 4)?,
    };
    let (mut month, mut day) = (1, 1);
    if bytes.get(index) == Some(&b'-') {
        index += 1;
        month = number(&mut index, 2)?;
        if bytes.get(index) == Some(&b'-') {
            index += 1;
            day = number(&mut index, 2)?;
        }
    }
    // V8은 월과 관계없이 1~31일을 받고 넘치는 날은 다음 달로 넘긴다(2026-02-30은 3월 2일).
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    let date = days_from_civil(year, month, day) * 86_400_000;
    if index == bytes.len() {
        return Some(date as f64);
    }
    if bytes.get(index) != Some(&b'T') {
        return None;
    }
    index += 1;
    let hour = number(&mut index, 2)?;
    if bytes.get(index) != Some(&b':') {
        return None;
    }
    index += 1;
    let minute = number(&mut index, 2)?;
    let (mut second, mut millisecond) = (0, 0);
    if bytes.get(index) == Some(&b':') {
        index += 1;
        second = number(&mut index, 2)?;
        if bytes.get(index) == Some(&b'.') {
            index += 1;
            let start = index;
            while bytes.get(index).is_some_and(u8::is_ascii_digit) {
                index += 1;
            }
            if index == start {
                return None;
            }
            let fraction = &text[start..index];
            millisecond = format!("{:0<3}", &fraction[..fraction.len().min(3)]).parse().ok()?;
        }
    }
    let offset_minutes = match bytes.get(index) {
        Some(b'Z') => {
            index += 1;
            0
        }
        Some(sign @ (b'+' | b'-')) => {
            let negative = *sign == b'-';
            index += 1;
            let offset_hour = number(&mut index, 2)?;
            if bytes.get(index) != Some(&b':') {
                return None;
            }
            index += 1;
            let offset_minute = number(&mut index, 2)?;
            if offset_hour > 23 || offset_minute > 59 {
                return None;
            }
            let total = offset_hour * 60 + offset_minute;
            if negative { -total } else { total }
        }
        _ => return None,
    };
    if index != bytes.len() || minute > 59 || second > 59 || hour > 24 {
        return None;
    }
    if hour == 24 && (minute != 0 || second != 0 || millisecond != 0) {
        return None;
    }
    let time = ((hour * 60 + minute - offset_minutes) * 60 + second) * 1_000 + millisecond;
    Some((date + time) as f64)
}

/// V8 MakeDay(year, month - 1, day)의 UTC 자정 밀리초. 1~31일이 그 달을 넘치면 다음 달로 넘긴다.
pub fn utc_day_milliseconds(year: i64, month: i64, day: i64) -> f64 {
    ((days_from_civil(year, month, 1) + day - 1) * 86_400_000) as f64
}

/// 밀리초 시각을 Date.prototype.toISOString 꼴로 쓴다. 0~9999년 밖은 JS처럼 부호와 여섯 자리 연도다.
pub fn iso_string(milliseconds: f64) -> String {
    let total = milliseconds as i64;
    let days = total.div_euclid(86_400_000);
    let rest = total.rem_euclid(86_400_000);
    let (year, month, day) = civil_from_days(days);
    let year = match year {
        0..=9_999 => format!("{year:04}"),
        _ if year < 0 => format!("-{:06}", -year),
        _ => format!("+{year:06}"),
    };
    format!(
        "{year}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{:03}Z",
        rest / 3_600_000,
        rest / 60_000 % 60,
        rest / 1_000 % 60,
        rest % 1_000
    )
}

fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let shifted = days + 719_468;
    let era = shifted.div_euclid(146_097);
    let day_of_era = shifted - era * 146_097;
    let year_of_era = (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_index + 2) / 5 + 1;
    let month = if month_index < 10 { month_index + 3 } else { month_index - 9 };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

/// Date.now().
pub fn now_milliseconds() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0.0, |elapsed| elapsed.as_millis() as f64)
}

/// Node os.tmpdir(). TMPDIR·TMP·TEMP 순서이고 끝의 '/' 하나를 뗀다.
pub fn node_tmpdir() -> String {
    let path = ["TMPDIR", "TMP", "TEMP"]
        .iter()
        .find_map(|key| std::env::var(key).ok().filter(|value| !value.is_empty()))
        .unwrap_or_else(|| "/tmp".into());
    match path.strip_suffix('/') {
        Some(trimmed) if path.len() > 1 => trimmed.to_string(),
        _ => path,
    }
}

/// crypto.randomUUID(). 버전 4 UUID다.
pub fn random_uuid() -> String {
    let mut bytes = [0_u8; 16];
    // SAFETY: 16바이트 버퍼를 운영체제 난수로 채운다.
    unsafe { libc::arc4random_buf(bytes.as_mut_ptr().cast(), bytes.len()) };
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let hex: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    format!("{}-{}-{}-{}-{}", &hex[..8], &hex[8..12], &hex[12..16], &hex[16..20], &hex[20..])
}

/// path.posix.normalize.
pub fn normalize_path(path: &str) -> String {
    if path.is_empty() {
        return ".".into();
    }
    let absolute = path.starts_with('/');
    let trailing = path.ends_with('/');
    let mut parts: Vec<&str> = Vec::new();
    for segment in path.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                if parts.last().is_some_and(|last| *last != "..") {
                    parts.pop();
                } else if !absolute {
                    parts.push("..");
                }
            }
            other => parts.push(other),
        }
    }
    let mut normalized = parts.join("/");
    if normalized.is_empty() {
        return if absolute {
            "/".into()
        } else if trailing {
            "./".into()
        } else {
            ".".into()
        };
    }
    if trailing {
        normalized.push('/');
    }
    if absolute { format!("/{normalized}") } else { normalized }
}

/// path.posix.join. 빈 인자는 건너뛰고 '/'로 이은 뒤 normalize한다.
pub fn join_path(parts: &[&str]) -> String {
    let joined = parts.iter().filter(|part| !part.is_empty()).copied().collect::<Vec<_>>().join("/");
    if joined.is_empty() { ".".into() } else { normalize_path(&joined) }
}

/// path.posix.extname.
pub fn extname(path: &str) -> &str {
    let trimmed = path.trim_end_matches('/');
    let base = trimmed.rsplit('/').next().unwrap_or("");
    match base.rfind('.') {
        None | Some(0) => "",
        Some(_) if base == ".." => "",
        Some(dot) => &base[dot..],
    }
}

/// `String(text).slice(0, units)`를 node-pg가 UTF-8로 보낼 때처럼 잘린 대리 문자는 U+FFFD가 된다.
pub fn utf16_prefix(text: &str, units: usize) -> String {
    let encoded: Vec<u16> = text.encode_utf16().take(units).collect();
    String::from_utf16_lossy(&encoded)
}

/// `String(text).slice(-units)`. 앞에서 잘린 대리 문자는 utf16_prefix처럼 U+FFFD가 된다.
pub fn utf16_suffix(text: &str, units: usize) -> String {
    let encoded: Vec<u16> = text.encode_utf16().collect();
    String::from_utf16_lossy(&encoded[encoded.len().saturating_sub(units)..])
}

/// Object.keys·Object.entries 순서. 배열 색인 모양 키(0 이상 2^32-1 미만의 정규 정수 글자)가 오름차순으로 먼저 오고
/// 나머지는 넣은 순서다. serde_json은 넣은 순서만 지키므로 순회 순서가 결과에 드러나는 곳에서 쓴다.
pub fn js_keys(object: &serde_json::Map<String, Value>) -> Vec<&str> {
    let index = |key: &str| {
        let canonical = !key.is_empty() && key.bytes().all(|byte| byte.is_ascii_digit()) && (key == "0" || !key.starts_with('0'));
        canonical.then(|| key.parse::<u64>().ok()).flatten().filter(|number| *number < 4_294_967_295)
    };
    let mut indexed: Vec<(u64, &str)> = object.keys().filter_map(|key| index(key).map(|number| (number, key.as_str()))).collect();
    indexed.sort_by_key(|(number, _)| *number);
    indexed
        .into_iter()
        .map(|(_, key)| key)
        .chain(object.keys().map(String::as_str).filter(|key| index(key).is_none()))
        .collect()
}

/// path.posix.resolve(path). 상대 경로는 현재 작업 폴더 기준이고 끝의 '/'는 지운다.
pub fn resolve_path(path: &str) -> String {
    let joined = if path.starts_with('/') {
        path.to_string()
    } else {
        let cwd = std::env::current_dir().map(|cwd| cwd.to_string_lossy().into_owned()).unwrap_or_else(|_| "/".into());
        format!("{cwd}/{path}")
    };
    let normalized = normalize_path(&joined);
    match normalized.trim_end_matches('/') {
        "" => "/".into(),
        trimmed => trimmed.to_string(),
    }
}

/// path.posix.resolve(from, to). `to`가 절대 경로면 그것만, 아니면 `from` 뒤에 이어 푼다.
pub fn resolve_paths(from: &str, to: &str) -> String {
    if to.starts_with('/') || from.is_empty() { resolve_path(to) } else { resolve_path(&format!("{from}/{to}")) }
}

/// Number.parseInt(text, 10). 앞 공백 뒤 부호와 숫자만 읽고, 숫자가 없으면 NaN이다.
pub fn parse_int(text: &str) -> f64 {
    let trimmed = text.trim_start_matches(is_js_whitespace);
    let (sign, digits) = match trimmed.as_bytes().first() {
        Some(b'-') => (-1.0, &trimmed[1..]),
        Some(b'+') => (1.0, &trimmed[1..]),
        _ => (1.0, trimmed),
    };
    let end = digits.bytes().position(|byte| !byte.is_ascii_digit()).unwrap_or(digits.len());
    if end == 0 {
        return f64::NAN;
    }
    sign * digits[..end].parse::<f64>().unwrap_or(f64::NAN)
}

/// Node fs 오류 문구 `ENOENT: no such file or directory, lstat '<경로>'`.
pub fn node_fs_message(error: &io::Error, syscall: &str, path: &str) -> String {
    let (code, description) = uv_error(error);
    format!("{code}: {description}, {syscall} '{path}'")
}

/// fs.realpathSync(path). Node의 JS 구현처럼 앞에서부터 경로 조각마다 lstat 하고, 심볼릭 링크를 만나면
/// stat·readlink 뒤 바뀐 경로를 처음부터 다시 푼다. 그래서 실패 문구의 경로는 실패한 부분 경로다
/// (예: `lstat '/private/tmp/없는-폴더'`).
pub fn realpath_sync(path: &str) -> Result<String, String> {
    use std::os::unix::fs::MetadataExt;
    let mut resolved = resolve_path(path);
    let mut known_hard = std::collections::HashSet::new();
    let mut seen_links: std::collections::HashMap<(u64, u64), String> = std::collections::HashMap::new();
    let mut current = String::from("/");
    let mut position = 1;
    while position < resolved.len() {
        let previous = current.clone();
        let rest = &resolved[position..];
        let base = match rest.find('/') {
            None => {
                current.push_str(rest);
                position = resolved.len();
                format!("{previous}{rest}")
            }
            Some(offset) => {
                current.push_str(&rest[..=offset]);
                position += offset + 1;
                format!("{previous}{}", &rest[..offset])
            }
        };
        if known_hard.contains(&base) {
            continue;
        }
        let metadata = std::fs::symlink_metadata(&base).map_err(|error| node_fs_message(&error, "lstat", &base))?;
        if !metadata.file_type().is_symlink() {
            known_hard.insert(base);
            continue;
        }
        let id = (metadata.dev(), metadata.ino());
        let target = match seen_links.get(&id) {
            Some(target) => target.clone(),
            None => {
                std::fs::metadata(&base).map_err(|error| node_fs_message(&error, "stat", &base))?;
                let target = std::fs::read_link(&base).map_err(|error| node_fs_message(&error, "readlink", &base))?;
                target.to_string_lossy().into_owned()
            }
        };
        let link = resolve_paths(&previous, &target);
        seen_links.insert(id, target);
        resolved = resolve_paths(&link, &resolved[position..]);
        current = String::from("/");
        position = 1;
    }
    Ok(resolved)
}

/// path.posix.relative(from, to). Node 구현의 공통 접두 탐색을 그대로 옮겼다('/'만 비교하므로 바이트 단위와 같다).
pub fn relative_path(from: &str, to: &str) -> String {
    if from == to {
        return String::new();
    }
    let from = resolve_path(from);
    let to = resolve_path(to);
    if from == to {
        return String::new();
    }
    let (from, to) = (from.as_bytes(), to.as_bytes());
    let (from_len, to_len) = (from.len() - 1, to.len() - 1);
    let length = from_len.min(to_len);
    let mut last_common_separator: isize = -1;
    let mut index = 0;
    while index < length {
        let code = from[1 + index];
        if code != to[1 + index] {
            break;
        }
        if code == b'/' {
            last_common_separator = index as isize;
        }
        index += 1;
    }
    let slice = |bytes: &[u8], start: usize| String::from_utf8_lossy(&bytes[start.min(bytes.len())..]).into_owned();
    if index == length {
        if to_len > length {
            if to[1 + index] == b'/' {
                return slice(to, 1 + index + 1);
            }
            if index == 0 {
                return slice(to, 1 + index);
            }
        } else if from_len > length {
            if from[1 + index] == b'/' {
                last_common_separator = index as isize;
            } else if index == 0 {
                last_common_separator = 0;
            }
        }
    }
    let mut out = String::new();
    let start = (1 + last_common_separator + 1) as usize;
    for position in start..=from.len() {
        if position == from.len() || from[position] == b'/' {
            out.push_str(if out.is_empty() { ".." } else { "/.." });
        }
    }
    out + &slice(to, (1 + last_common_separator) as usize)
}

/// libuv 오류 이름과 설명(uv_err_name·uv_strerror). Node 오류 문구의 `ENOENT: no such file or directory` 부분이다.
/// 표에 없는 번호는 libuv처럼 UNKNOWN이다.
pub fn uv_error(error: &io::Error) -> (&'static str, &'static str) {
    match error.raw_os_error() {
        Some(libc::ENOENT) => ("ENOENT", "no such file or directory"),
        Some(libc::EACCES) => ("EACCES", "permission denied"),
        Some(libc::ENOTDIR) => ("ENOTDIR", "not a directory"),
        Some(libc::EPERM) => ("EPERM", "operation not permitted"),
        Some(libc::EISDIR) => ("EISDIR", "illegal operation on a directory"),
        Some(libc::ENOEXEC) => ("ENOEXEC", "exec format error"),
        Some(libc::ELOOP) => ("ELOOP", "too many symbolic links encountered"),
        Some(libc::ENAMETOOLONG) => ("ENAMETOOLONG", "name too long"),
        Some(libc::EMFILE) => ("EMFILE", "too many open files"),
        Some(libc::ENFILE) => ("ENFILE", "file table overflow"),
        Some(libc::E2BIG) => ("E2BIG", "argument list too long"),
        Some(libc::ENOMEM) => ("ENOMEM", "not enough memory"),
        Some(libc::EAGAIN) => ("EAGAIN", "resource temporarily unavailable"),
        Some(libc::EIO) => ("EIO", "i/o error"),
        _ => ("UNKNOWN", "unknown error"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_uri_component는_node와_같다() {
        // node -e 'console.log(encodeURIComponent("a b/c?d=é&직원-_.!~*\x27()#"))'
        assert_eq!(
            encode_uri_component("a b/c?d=é&직원-_.!~*'()#"),
            "a%20b%2Fc%3Fd%3D%C3%A9%26%EC%A7%81%EC%9B%90-_.!~*'()%23"
        );
        assert_eq!(decode_uri_component(&encode_uri_component("직원 1/2")).as_deref(), Some("직원 1/2"));
    }

    #[test]
    fn parse_int와_resolve_paths는_node와_같다() {
        assert_eq!(parse_int("  12abc"), 12.0);
        assert_eq!(parse_int("+7"), 7.0);
        assert_eq!(parse_int("-3\t4"), -3.0);
        assert!(parse_int("-").is_nan());
        assert!(parse_int("x1").is_nan());
        assert!(parse_int("").is_nan());
        assert_eq!(resolve_paths("/repo/a", "../x"), "/repo/x");
        assert_eq!(resolve_paths("/repo", "/abs/./y"), "/abs/y");
    }

    /// 기대 문구는 Node 24 `fs.realpathSync`를 같은 경로로 실행해 얻은 값이다.
    #[test]
    fn realpath는_node처럼_부분_경로로_실패를_알린다() {
        let root = format!("/tmp/js-realpath-{}", std::process::id());
        std::fs::create_dir_all(format!("{root}/real")).unwrap();
        std::fs::write(format!("{root}/real/file.txt"), "x").unwrap();
        std::os::unix::fs::symlink(format!("{root}/real"), format!("{root}/linked")).unwrap();
        std::os::unix::fs::symlink(format!("{root}/none"), format!("{root}/dangling")).unwrap();
        let private = format!("/private{root}");

        assert_eq!(realpath_sync(&format!("{root}/linked/file.txt")), Ok(format!("{private}/real/file.txt")));
        assert_eq!(realpath_sync("/"), Ok("/".to_string()));
        assert_eq!(
            realpath_sync(&format!("{root}/missing-dir/a.png")),
            Err(format!("ENOENT: no such file or directory, lstat '{private}/missing-dir'"))
        );
        assert_eq!(
            realpath_sync(&format!("{root}/real/file.txt/x")),
            Err(format!("ENOTDIR: not a directory, lstat '{private}/real/file.txt/x'"))
        );
        assert_eq!(
            realpath_sync(&format!("{root}/dangling")),
            Err(format!("ENOENT: no such file or directory, stat '{private}/dangling'"))
        );
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn 숫자_변환은_js_number를_따른다() {
        assert_eq!(string_to_number(""), 0.0);
        assert_eq!(string_to_number("  30 \n"), 30.0);
        assert_eq!(string_to_number("0x20"), 32.0);
        assert_eq!(string_to_number("0b101"), 5.0);
        assert_eq!(string_to_number("1e2"), 100.0);
        assert_eq!(string_to_number(".5"), 0.5);
        assert_eq!(string_to_number("5."), 5.0);
        assert_eq!(string_to_number("-Infinity"), f64::NEG_INFINITY);
        for invalid in ["abc", "-0x10", "0x", "1e", "inf", "NaN", "1_000", "."] {
            assert!(string_to_number(invalid).is_nan(), "{invalid}");
        }
    }

    #[test]
    fn 숫자_표기는_js_tostring을_따른다() {
        let cases = [
            (0.00005, "0.00005"),
            (0.0000001, "1e-7"),
            (1.0, "1"),
            (-0.0, "0"),
            (123.456, "123.456"),
            (1e21, "1e+21"),
            (1.5e21, "1.5e+21"),
            (123456789012345680000.0, "123456789012345680000"),
            (0.1 + 0.2, "0.30000000000000004"),
            (25.5, "25.5"),
            (f64::NAN, "NaN"),
        ];
        for (value, expected) in cases {
            assert_eq!(number_to_string(value), expected);
        }
        assert_eq!(to_js_json(&serde_json::json!({ "a": 1.0, "b": 5e-5, "c": [2, -0.0] })).unwrap(), br#"{"a":1,"b":0.00005,"c":[2,0]}"#);
        // Node v24: JSON.stringify(JSON.parse(...))의 결과.
        let big: serde_json::Value =
            serde_json::from_str(r#"{"a":12345678901234567890,"b":-9007199254740993,"c":9007199254740991}"#).unwrap();
        assert_eq!(to_js_json(&big).unwrap(), br#"{"a":12345678901234567000,"b":-9007199254740992,"c":9007199254740991}"#);
        // 운영 ts_rank 점수. 기본 serde_json 해석은 1ULP 어긋나 0.0936066508293152가 된다.
        let float: serde_json::Value =
            serde_json::from_str(r#"{"s":0.09360665082931519,"t":2.2250738585072011e-308,"u":0.1,"v":1.7976931348623157e308}"#)
                .unwrap();
        assert_eq!(to_js_json(&float).unwrap(), br#"{"s":0.09360665082931519,"t":2.225073858507201e-308,"u":0.1,"v":1.7976931348623157e+308}"#);
    }

    #[test]
    fn json_값의_string_number_pg_변환은_node와_같다() {
        // Node v24 String(v)·Number(x)·pg/lib/utils prepareValue 결과.
        let values = serde_json::json!([1, [2, null], "a\"b\\c", { "k": [1] }, true, 1e21, -0.5]);
        assert_eq!(js_string(&values), "1,2,,a\"b\\c,[object Object],true,1e+21,-0.5");
        let numbers: Vec<f64> = [serde_json::json!([7]), serde_json::json!([]), serde_json::json!([1, 2]), serde_json::json!("0x10"), serde_json::json!({}), serde_json::json!(true), Value::Null, serde_json::json!(" 12 ")]
            .iter()
            .map(js_to_number)
            .collect();
        assert_eq!(numbers[..2], [7.0, 0.0]);
        assert!(numbers[2].is_nan() && numbers[4].is_nan());
        assert_eq!([numbers[3], numbers[5], numbers[6], numbers[7]], [16.0, 1.0, 0.0, 12.0]);
        assert_eq!(pg_text(&values).unwrap(), r#"{"1",{"2",NULL},"a\"b\\c","{\"k\":[1]}","true","1e+21","-0.5"}"#);
        assert_eq!(pg_text(&serde_json::json!({ "a": "x", "b": [1, null] })).unwrap(), r#"{"a":"x","b":[1,null]}"#);
        assert_eq!(pg_text(&serde_json::json!(5e-7)).unwrap(), "5e-7");
        assert_eq!(pg_text(&serde_json::json!(false)).unwrap(), "false");
        assert_eq!(pg_text(&Value::Null), None);
        assert_eq!(js_property(&Value::Null, "query").unwrap_err(), "Cannot read properties of null (reading 'query')");
        assert_eq!(js_property(&serde_json::json!([1]), "query").unwrap(), None);
    }

    #[test]
    fn 들여쓴_json은_json_stringify_두_칸과_같다() {
        // Node v24 JSON.stringify(value, null, 2) 결과.
        let value = serde_json::json!({ "a": [], "b": {}, "c": [1, { "d": 0.00005, "n": null }], "e": "x\n", "f": 1e21 });
        assert_eq!(
            to_js_json_pretty(&value),
            "{\n  \"a\": [],\n  \"b\": {},\n  \"c\": [\n    1,\n    {\n      \"d\": 0.00005,\n      \"n\": null\n    }\n  ],\n  \"e\": \"x\\n\",\n  \"f\": 1e+21\n}"
        );
    }

    #[test]
    fn 날짜는_iso_꼴만_읽는다() {
        assert_eq!(date_parse("2026-08-20T01:02:03.000Z"), Some(1_787_187_723_000.0));
        assert_eq!(date_parse("2026-08-28T00:00:00Z"), Some(1_787_875_200_000.0));
        assert_eq!(date_parse("2026-08-28"), Some(1_787_875_200_000.0));
        assert_eq!(date_parse("2026-08-28T09:00:00+09:00"), Some(1_787_875_200_000.0));
        assert_eq!(date_parse("2026-08-28T00:00:00.123456Z"), Some(1_787_875_200_123.0));
        assert_eq!(date_parse("2026-02-30"), Some(1_772_409_600_000.0));
        assert_eq!(date_parse("2026-02-32"), None);
        assert_eq!(date_parse("nope"), None);
        assert_eq!(iso_string(1_787_875_200_123.0), "2026-08-28T00:00:00.123Z");
    }

    #[test]
    fn 경로는_posix_규칙으로_정리한다() {
        assert_eq!(normalize_path("/a//b/../c/./d.png"), "/a/c/d.png");
        assert_eq!(normalize_path("/../a/"), "/a/");
        assert_eq!(join_path(&["/root", "session/../x"]), "/root/x");
        assert_eq!(extname("/a/b.PNG"), ".PNG");
        assert_eq!(extname("/a/.png"), "");
        assert_eq!(extname("/a/..png"), ".png");
        assert_eq!(extname("/a/b.png/"), ".png");
        assert_eq!(extname("/a/.."), "");
    }

    // 기대값은 Node path.posix.relative·Object.keys·JSON.parse·slice 결과다.
    #[test]
    fn 상대_경로와_키_순서와_대리_문자는_node를_따른다() {
        let relative = [
            ("/Users/example/office", "/Users/example/office/src/a.rs", "src/a.rs"),
            ("/a/b", "/a", ".."),
            ("/a/b/c", "/a/x", "../../x"),
            ("/", "/foo", "foo"),
            ("/foo/bar", "/", "../.."),
            ("/a/", "/a", ""),
            ("/a/bc", "/a/b", "../b"),
            ("/a/b", "/a/bc/d", "../bc/d"),
        ];
        for (from, to, expected) in relative {
            assert_eq!(relative_path(from, to), expected, "{from} → {to}");
        }
        let object: Value = serde_json::from_str(r#"{"b":1,"10":2,"2":3,"01":4,"4294967295":5,"4294967294":6}"#).unwrap();
        assert_eq!(js_keys(object.as_object().unwrap()), ["2", "10", "4294967294", "b", "01", "4294967295"]);
        // Node는 짝 없는 대리 문자를 그대로 들고, Rust는 U+FFFD로 읽는다. 이스케이프된 역슬래시 뒤 글자는 건드리지 않는다.
        let parsed = parse_js_json(r#"["\\ud83d","\ud83d\ude00","\udc00x","\uD83D\u0041"]"#).unwrap();
        assert_eq!(parsed, serde_json::json!(["\\ud83d", "😀", "\u{fffd}x", "\u{fffd}A"]));
        assert!(parse_js_json(r#"{"a":"\ud83d"#).is_err());
        assert_eq!(utf16_suffix("😀😀", 3), "\u{fffd}😀");
    }
}
