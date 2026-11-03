// 리스(RisuAI) 프리셋(.risup)·모듈(.risum) 파일을 풀어 값으로 돌려준다. Node roleplay-risu-format.mjs와 같은 오류 문구를 쓴다.

use std::io::Read;
use std::sync::LazyLock;

use flate2::read::MultiGzDecoder;
use ring::aead::{AES_256_GCM, Aad, LessSafeKey, Nonce, UnboundKey};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

use super::js_number;
use crate::js::{number_to_string, parse_js_json};

// RisuAI(GPL-3.0, kwaroran/RisuAI src/ts/rpack/rpack_map.bin)의 바이트 변환표 중 해독 절반.
const RPACK_DECODE_HEX: &str = concat!(
    "2cf7848bc965fbb69faeb3032d0169741fe4a3ecee5c3421934a0f6ae262029e",
    "229cfd3cfc71c7c6ad596705706d8a4412fa24865fafd17a47cefe5063dd5106",
    "6f18e052a8099d56734cb8536cc3a00e19cf3e0d7e07326846ea48f9992eaba4",
    "49205e5535380cbcd3b1581679280a1ae1f2cdc439dba2ba6072767d95ef7fc8",
    "c0de3794bfb51481922545ace7f566a72b365ac113e34b3ae88d831b7c27b09a",
    "42eb87aadc548e7826d25729d4b7f82f8f8975f04177c21effd81511e5049717",
    "f331d09b00d7cab44f2a3bd9b26bda5da13f3061bd913d4ee6dfbe4d828c1d23",
    "109864f485337b9043bba988f1d6a51cf6cc6eb95b0b96edd5e9c5cb08a68040",
);

static RPACK_DECODE: LazyLock<[u8; 256]> = LazyLock::new(|| {
    let mut table = [0_u8; 256];
    for (index, byte) in table.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&RPACK_DECODE_HEX[index * 2..index * 2 + 2], 16).unwrap_or_default();
    }
    table
});

/// msgpack을 이만큼보다 깊게 겹치면 해독 실패로 본다. Node의 호출 스택 초과(RangeError)에 해당한다.
const MAX_DEPTH: usize = 1000;

/// RisuFormatError. 문구가 그대로 400 응답이 된다.
#[derive(Debug, Clone, PartialEq)]
pub struct RisuFormatError(pub String);

impl std::fmt::Display for RisuFormatError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

fn format_error(message: &str) -> RisuFormatError {
    RisuFormatError(message.to_string())
}

/// 해독 중 실패. Format은 RisuFormatError, Other는 그 밖의 JS 오류(바깥에서 일반 문구로 바뀐다)다.
#[derive(Debug)]
enum Failure {
    Format(RisuFormatError),
    Other,
}

impl From<RisuFormatError> for Failure {
    fn from(error: RisuFormatError) -> Self {
        Self::Format(error)
    }
}

pub fn decode_rpack(data: &[u8]) -> Vec<u8> {
    data.iter().map(|byte| RPACK_DECODE[usize::from(*byte)]).collect()
}

/// 시험용: 리스와 같은 방식으로 묶는다.
pub fn encode_rpack(data: &[u8]) -> Vec<u8> {
    let mut encode = [0_u8; 256];
    for (index, byte) in RPACK_DECODE.iter().enumerate() {
        encode[usize::from(*byte)] = index as u8;
    }
    data.iter().map(|byte| encode[usize::from(*byte)]).collect()
}

/// msgpack에서 읽은 JS 값. undefined와 Buffer를 JSON 값과 구분한다.
#[derive(Debug, Clone, PartialEq)]
pub enum Msg {
    Undefined,
    Null,
    Bool(bool),
    Number(f64),
    String(String),
    Binary(Vec<u8>),
    Array(Vec<Msg>),
    /// 객체. 같은 키를 다시 쓰면 처음 자리에서 값만 바뀐다.
    Map(Vec<(String, Msg)>),
}

impl Msg {
    /// `value.key`. 객체가 아니거나 키가 없으면 undefined다.
    pub fn get(&self, key: &str) -> &Msg {
        match self {
            Msg::Map(entries) => entries.iter().find(|(name, _)| name == key).map_or(&Msg::Undefined, |(_, value)| value),
            _ => &Msg::Undefined,
        }
    }

    fn insert(entries: &mut Vec<(String, Msg)>, key: String, value: Msg) {
        // `obj["__proto__"] = x`는 자기 속성을 만들지 않는다.
        if key == "__proto__" {
            return;
        }
        match entries.iter_mut().find(|(name, _)| *name == key) {
            Some(entry) => entry.1 = value,
            None => entries.push((key, value)),
        }
    }

    /// JS `String(value)`. Buffer는 UTF-8로 읽는다.
    pub fn js_string(&self) -> String {
        match self {
            Msg::Undefined => "undefined".into(),
            Msg::Null => "null".into(),
            Msg::Bool(flag) => flag.to_string(),
            Msg::Number(number) => number_to_string(*number),
            Msg::String(text) => text.clone(),
            Msg::Binary(bytes) => String::from_utf8_lossy(bytes).into_owned(),
            Msg::Array(items) => items
                .iter()
                .map(|item| if matches!(item, Msg::Undefined | Msg::Null) { String::new() } else { item.js_string() })
                .collect::<Vec<_>>()
                .join(","),
            Msg::Map(_) => "[object Object]".into(),
        }
    }

    fn truthy(&self) -> bool {
        match self {
            Msg::Undefined | Msg::Null => false,
            Msg::Bool(flag) => *flag,
            Msg::Number(number) => *number != 0.0 && !number.is_nan(),
            Msg::String(text) => !text.is_empty(),
            _ => true,
        }
    }

    /// `JSON.parse(JSON.stringify(value))`. undefined 속성은 빠지고 배열 안 undefined는 null, Buffer는 {type, data}다.
    pub fn to_json(&self) -> Value {
        match self {
            Msg::Undefined | Msg::Null => Value::Null,
            Msg::Bool(flag) => Value::Bool(*flag),
            Msg::Number(number) => js_number(*number),
            Msg::String(text) => Value::String(text.clone()),
            Msg::Binary(bytes) => {
                let mut buffer = Map::new();
                buffer.insert("type".into(), Value::String("Buffer".into()));
                buffer.insert("data".into(), Value::Array(bytes.iter().map(|byte| Value::from(*byte)).collect()));
                Value::Object(buffer)
            }
            Msg::Array(items) => Value::Array(items.iter().map(Msg::to_json).collect()),
            Msg::Map(entries) => Value::Object(
                entries
                    .iter()
                    .filter(|(_, value)| *value != Msg::Undefined)
                    .map(|(key, value)| (key.clone(), value.to_json()))
                    .collect(),
            ),
        }
    }

    /// 시험용: JSON 값을 같은 모양의 msgpack 값으로 바꾼다.
    pub fn from_json(value: &Value) -> Msg {
        match value {
            Value::Null => Msg::Null,
            Value::Bool(flag) => Msg::Bool(*flag),
            Value::Number(number) => Msg::Number(number.as_f64().unwrap_or(f64::NAN)),
            Value::String(text) => Msg::String(text.clone()),
            Value::Array(items) => Msg::Array(items.iter().map(Msg::from_json).collect()),
            Value::Object(fields) => Msg::Map(fields.iter().map(|(key, value)| (key.clone(), Msg::from_json(value))).collect()),
        }
    }
}

/// 리스가 쓰는 표준 msgpack만 읽는다. 확장 타입은 msgpackr의 undefined 표기만 받는다.
pub fn decode_msgpack(data: &[u8]) -> Result<Msg, RisuFormatError> {
    match decode_msgpack_value(data) {
        Ok(value) => Ok(value),
        Err(Failure::Format(error)) => Err(error),
        Err(Failure::Other) => Err(format_error("msgpack 데이터를 읽지 못했습니다.")),
    }
}

fn decode_msgpack_value(data: &[u8]) -> Result<Msg, Failure> {
    let mut reader = Reader { data, position: 0 };
    let value = reader.read(0)?;
    if reader.position != data.len() {
        return Err(format_error("msgpack 뒤에 남은 데이터가 있습니다.").into());
    }
    Ok(value)
}

struct Reader<'a> {
    data: &'a [u8],
    position: usize,
}

impl Reader<'_> {
    fn bytes(&mut self, length: usize) -> Result<&[u8], Failure> {
        if self.position + length > self.data.len() {
            return Err(format_error("msgpack 데이터가 잘렸습니다.").into());
        }
        let value = &self.data[self.position..self.position + length];
        self.position += length;
        Ok(value)
    }

    fn array<const N: usize>(&mut self) -> Result<[u8; N], Failure> {
        let mut value = [0_u8; N];
        value.copy_from_slice(self.bytes(N)?);
        Ok(value)
    }

    fn length(&mut self, width: usize) -> Result<usize, Failure> {
        Ok(match width {
            1 => usize::from(self.array::<1>()?[0]),
            2 => usize::from(u16::from_be_bytes(self.array()?)),
            _ => u32::from_be_bytes(self.array()?) as usize,
        })
    }

    fn string(&mut self, length: usize) -> Result<Msg, Failure> {
        Ok(Msg::String(String::from_utf8_lossy(self.bytes(length)?).into_owned()))
    }

    fn list(&mut self, length: usize, depth: usize) -> Result<Msg, Failure> {
        let mut items = Vec::new();
        for _ in 0..length {
            items.push(self.read(depth + 1)?);
        }
        Ok(Msg::Array(items))
    }

    fn map(&mut self, length: usize, depth: usize) -> Result<Msg, Failure> {
        let mut entries = Vec::new();
        for _ in 0..length {
            let key = self.read(depth + 1)?.js_string();
            let value = self.read(depth + 1)?;
            Msg::insert(&mut entries, key, value);
        }
        Ok(Msg::Map(entries))
    }

    fn read(&mut self, depth: usize) -> Result<Msg, Failure> {
        if depth > MAX_DEPTH {
            return Err(Failure::Other);
        }
        let kind = self.array::<1>()?[0];
        match kind {
            0x00..=0x7f => return Ok(Msg::Number(f64::from(kind))),
            0x80..=0x8f => return self.map(usize::from(kind & 0x0f), depth),
            0x90..=0x9f => return self.list(usize::from(kind & 0x0f), depth),
            0xa0..=0xbf => return self.string(usize::from(kind & 0x1f)),
            0xe0..=0xff => return Ok(Msg::Number(f64::from(i16::from(kind) - 0x100))),
            _ => {}
        }
        Ok(match kind {
            0xc0 => Msg::Null,
            0xc2 => Msg::Bool(false),
            0xc3 => Msg::Bool(true),
            0xc4 | 0xc5 | 0xc6 => {
                let width = [1, 2, 4][usize::from(kind - 0xc4)];
                let length = self.length(width)?;
                Msg::Binary(self.bytes(length)?.to_vec())
            }
            0xca => Msg::Number(f64::from(f32::from_be_bytes(self.array()?))),
            0xcb => Msg::Number(f64::from_be_bytes(self.array()?)),
            0xcc => Msg::Number(f64::from(self.array::<1>()?[0])),
            0xcd => Msg::Number(f64::from(u16::from_be_bytes(self.array()?))),
            0xce => Msg::Number(f64::from(u32::from_be_bytes(self.array()?))),
            0xcf => Msg::Number(u64::from_be_bytes(self.array()?) as f64),
            0xd0 => Msg::Number(f64::from(i8::from_be_bytes(self.array()?))),
            0xd1 => Msg::Number(f64::from(i16::from_be_bytes(self.array()?))),
            0xd2 => Msg::Number(f64::from(i32::from_be_bytes(self.array()?))),
            0xd3 => Msg::Number(i64::from_be_bytes(self.array()?) as f64),
            0xd9 => {
                let length = self.length(1)?;
                self.string(length)?
            }
            0xda => {
                let length = self.length(2)?;
                self.string(length)?
            }
            0xdb => {
                let length = self.length(4)?;
                self.string(length)?
            }
            0xdc => {
                let length = self.length(2)?;
                self.list(length, depth)?
            }
            0xdd => {
                let length = self.length(4)?;
                self.list(length, depth)?
            }
            0xde => {
                let length = self.length(2)?;
                self.map(length, depth)?
            }
            0xdf => {
                let length = self.length(4)?;
                self.map(length, depth)?
            }
            0xd4 => {
                // msgpackr는 undefined를 fixext1(타입 0, 값 0)으로 쓴다.
                let [ext_type, value] = self.array::<2>()?;
                if ext_type == 0 && value == 0 {
                    Msg::Undefined
                } else {
                    return Err(format_error("지원하지 않는 msgpack 확장 타입입니다.").into());
                }
            }
            other => return Err(RisuFormatError(format!("지원하지 않는 msgpack 타입입니다: 0x{other:x}")).into()),
        })
    }
}

fn gunzip(data: &[u8]) -> Result<Vec<u8>, Failure> {
    // Node gunzipSync는 빈 입력을 "unexpected end of file"로 거절한다.
    if data.is_empty() {
        return Err(Failure::Other);
    }
    let mut output = Vec::new();
    MultiGzDecoder::new(data).read_to_end(&mut output).map_err(|_| Failure::Other)?;
    Ok(output)
}

/// `Buffer.from(data)`. 문자열은 UTF-8, 배열은 각 값의 하위 8비트이고 그 밖은 TypeError다.
fn buffer_from(value: &Msg) -> Result<Vec<u8>, Failure> {
    match value {
        Msg::Binary(bytes) => Ok(bytes.clone()),
        Msg::String(text) => Ok(text.as_bytes().to_vec()),
        Msg::Array(items) => Ok(items
            .iter()
            .map(|item| match item {
                Msg::Number(number) if number.is_finite() => (number.trunc() as i64 & 0xff) as u8,
                Msg::Bool(true) => 1,
                Msg::String(text) => {
                    let number = crate::js::string_to_number(text);
                    if number.is_finite() { (number.trunc() as i64 & 0xff) as u8 } else { 0 }
                }
                _ => 0,
            })
            .collect()),
        _ => Err(Failure::Other),
    }
}

fn decrypt_preset_buffer(data: &Msg) -> Result<Vec<u8>, Failure> {
    let mut buffer = buffer_from(data)?;
    if buffer.len() < 16 {
        return Err(format_error("암호화된 프리셋이 너무 짧습니다.").into());
    }
    let key = Sha256::digest(b"risupreset");
    let key = LessSafeKey::new(UnboundKey::new(&AES_256_GCM, &key).map_err(|_| Failure::Other)?);
    let plain = key
        .open_in_place(Nonce::assume_unique_for_key([0; 12]), Aad::empty(), &mut buffer)
        .map_err(|_| Failure::Other)?;
    Ok(plain.to_vec())
}

/// decodeRisuPreset. 프롬프트 템플릿 배열이 있는 객체만 받는다.
pub fn decode_risu_preset(file: &[u8]) -> Result<Msg, RisuFormatError> {
    let outer = gunzip(&decode_rpack(file)).and_then(|inflated| decode_msgpack_value(&inflated));
    let outer = match outer {
        Ok(outer) => outer,
        Err(Failure::Format(error)) => return Err(error),
        Err(Failure::Other) => return Err(format_error("리스 프리셋(.risup) 파일이 아닙니다.")),
    };
    let version_ok = matches!(outer.get("presetVersion"), Msg::Number(version) if *version == 0.0 || *version == 2.0);
    if *outer.get("type") != Msg::String("preset".into()) || !version_ok {
        return Err(format_error("지원하지 않는 리스 프리셋 버전입니다."));
    }
    let encrypted = match outer.get("preset") {
        Msg::Undefined | Msg::Null => outer.get("pres"),
        value => value,
    };
    if !encrypted.truthy() {
        return Err(format_error("프리셋 본문이 없습니다."));
    }
    let preset = match decrypt_preset_buffer(encrypted).and_then(|plain| decode_msgpack_value(&plain)) {
        Ok(preset) => preset,
        Err(Failure::Format(error)) => return Err(error),
        Err(Failure::Other) => return Err(format_error("프리셋 본문을 풀지 못했습니다.")),
    };
    if !matches!(preset, Msg::Map(_)) || !matches!(preset.get("promptTemplate"), Msg::Array(_)) {
        return Err(format_error("프롬프트 템플릿이 있는 프리셋만 지원합니다."));
    }
    Ok(preset)
}

/// decodeRisuModule. 6바이트 머리(111, 0, 길이 u32 LE) 뒤의 rpack JSON에서 module을 꺼낸다.
pub fn decode_risu_module(file: &[u8]) -> Result<Value, RisuFormatError> {
    if file.len() < 6 || file[0] != 111 || file[1] != 0 {
        return Err(format_error("리스 모듈(.risum) 파일이 아닙니다."));
    }
    let length = u32::from_le_bytes([file[2], file[3], file[4], file[5]]) as usize;
    if 6 + length > file.len() {
        return Err(format_error("모듈 데이터가 잘렸습니다."));
    }
    let text = String::from_utf8_lossy(&decode_rpack(&file[6..6 + length])).into_owned();
    let main = parse_js_json(&text).map_err(|_| format_error("모듈 본문을 풀지 못했습니다."))?;
    let module = main.get("module").filter(|module| crate::js::js_truthy(Some(module)));
    match (main.get("type"), module) {
        (Some(Value::String(kind)), Some(module)) if kind == "risuModule" => Ok(module.clone()),
        _ => Err(format_error("리스 모듈 형식이 아닙니다.")),
    }
}

/// 시험용 파일 만들기. Node 시험의 encodeMsgpack·risuPresetFile·risuModuleFile과 같은 바이트 규칙이다.
#[cfg(test)]
pub(crate) mod fixtures {
    use std::io::Write;

    use flate2::Compression;
    use flate2::write::GzEncoder;
    use ring::aead::{AES_256_GCM, Aad, LessSafeKey, Nonce, UnboundKey};
    use serde_json::{Value, json};
    use sha2::{Digest, Sha256};

    use super::{Msg, encode_rpack};

    pub fn encode_msgpack(value: &Msg) -> Vec<u8> {
        let mut output = Vec::new();
        write(value, &mut output);
        output
    }

    fn write(value: &Msg, output: &mut Vec<u8>) {
        match value {
            Msg::Undefined => output.extend([0xd4, 0, 0]),
            Msg::Null => output.push(0xc0),
            Msg::Bool(flag) => output.push(if *flag { 0xc3 } else { 0xc2 }),
            Msg::Number(number) => {
                let integer = number.fract() == 0.0 && number.is_finite();
                if integer && *number >= 0.0 && *number < 128.0 {
                    output.push(*number as u8);
                } else if integer && *number >= -(2_f64.powi(31)) && *number < 2_f64.powi(31) {
                    output.push(0xd2);
                    output.extend((*number as i32).to_be_bytes());
                } else {
                    output.push(0xcb);
                    output.extend(number.to_be_bytes());
                }
            }
            Msg::String(text) => {
                let bytes = text.as_bytes();
                if bytes.len() < 32 {
                    output.push(0xa0 | bytes.len() as u8);
                } else {
                    output.push(0xdb);
                    output.extend((bytes.len() as u32).to_be_bytes());
                }
                output.extend(bytes);
            }
            Msg::Binary(bytes) => {
                output.push(0xc6);
                output.extend((bytes.len() as u32).to_be_bytes());
                output.extend(bytes);
            }
            Msg::Array(items) => {
                output.push(0xdd);
                output.extend((items.len() as u32).to_be_bytes());
                for item in items {
                    write(item, output);
                }
            }
            Msg::Map(entries) => {
                output.push(0xdf);
                output.extend((entries.len() as u32).to_be_bytes());
                for (key, entry) in entries {
                    write(&Msg::String(key.clone()), output);
                    write(entry, output);
                }
            }
        }
    }

    pub fn gzip(data: &[u8]) -> Vec<u8> {
        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(data).unwrap();
        encoder.finish().unwrap()
    }

    pub fn risu_preset_file(preset: &Msg) -> Vec<u8> {
        let key = Sha256::digest(b"risupreset");
        let key = LessSafeKey::new(UnboundKey::new(&AES_256_GCM, &key).unwrap());
        let mut encrypted = encode_msgpack(preset);
        key.seal_in_place_append_tag(Nonce::assume_unique_for_key([0; 12]), Aad::empty(), &mut encrypted).unwrap();
        let outer = Msg::Map(vec![
            ("presetVersion".into(), Msg::Number(2.0)),
            ("type".into(), Msg::String("preset".into())),
            ("preset".into(), Msg::Binary(encrypted)),
        ]);
        encode_rpack(&gzip(&encode_msgpack(&outer)))
    }

    pub fn risu_module_file(module: &Value) -> Vec<u8> {
        let body = encode_rpack(json!({ "type": "risuModule", "module": module }).to_string().as_bytes());
        let mut file = vec![111, 0];
        file.extend((body.len() as u32).to_le_bytes());
        file.extend(body);
        file
    }

    /// 블록 순서·토글 조건·정규식·플래그를 고루 쓰는 작은 프리셋(JSON 부분). namespace로 통합 모듈 이름을 바꾼다.
    pub fn synthetic_preset_json(namespace: &str) -> Value {
        json!({
            "name": "합성 프리셋",
            "promptTemplate": [
                { "type": "plain", "type2": "main", "role": "system",
                  "text": "시작 {{#if_pure {{? {{getglobalvar::toggle_on}}=1}}}}켜짐{{/if}}{{position::x}}" },
                { "type": "description", "innerFormat": "<설명>{{slot}}</설명>" },
                { "type": "chat", "rangeStart": 0, "rangeEnd": -1 },
                { "type": "plain", "type2": "normal", "role": "system", "text": "중간 지시 {{user}}" },
                { "type": "chat", "rangeStart": -1, "rangeEnd": "end" },
                { "type": "plain", "type2": "normal", "role": "bot", "text": "알겠어요" },
            ],
            "customPromptTemplateToggle": "on=켜기\nmode=방식=select=짧게,길게\n=묶음=group",
            "moduleIntergration": namespace,
            "maxResponse": 1000,
            "temperature": 80,
            "thinkingType": "budget",
            "thinkingTokens": 500,
            "enableCustomFlags": true,
            "customFlags": [7],
            "systemContentReplacement": "",
            "systemRoleReplacement": "user",
            "promptSettings": { "trimStartNewChat": false },
            "regex": [{ "comment": "", "type": "editprocess", "in": "비밀", "out": "(가림)", "flag": "g", "ableFlag": false }],
        })
    }

    /// Node syntheticPreset(): JSON 부분 끝에 값이 undefined인 localStopStrings가 붙는다.
    pub fn synthetic_preset(namespace: &str) -> Msg {
        let Msg::Map(mut entries) = Msg::from_json(&synthetic_preset_json(namespace)) else { unreachable!() };
        entries.push(("localStopStrings".into(), Msg::Undefined));
        Msg::Map(entries)
    }

    pub fn synthetic_module(namespace: &str) -> Value {
        json!({
            "name": "합성 모듈",
            "namespace": namespace,
            "regex": [
                { "comment": "", "type": "editoutput", "in": "(\\d+)", "out": "[$1]", "flag": "g", "ableFlag": true },
                { "comment": "", "type": "editinput", "in": "^/", "out": "", "flag": "g", "ableFlag": true },
            ],
        })
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use sha2::{Digest, Sha256};

    use super::fixtures::*;
    use super::*;

    #[test]
    fn rpack_표는_리스_원본과_같고_되돌릴_수_있다() {
        let all: Vec<u8> = (0..=255).collect();
        // Node decodeRPack(0..255)의 sha256.
        let digest: String = Sha256::digest(decode_rpack(&all)).iter().map(|byte| format!("{byte:02x}")).collect();
        assert_eq!(digest, "541399aa080429e113232c7e33190d61b2c10d3505a7eb4769bd778a84d18989");
        assert_eq!(decode_rpack(&encode_rpack(&all)), all);
    }

    #[test]
    fn 리스_프리셋_모듈_파일을_원래_객체로_푼다() {
        let preset = synthetic_preset("test-module");
        let decoded = decode_risu_preset(&risu_preset_file(&preset)).unwrap();
        assert_eq!(decoded, preset);
        assert_eq!(decoded.get("customFlags").to_json(), json!([7]));
        assert_eq!(*decoded.get("temperature"), Msg::Number(80.0));
        // JS 객체에는 키가 남고 값이 undefined다. 저장하는 JSON에서는 빠진다.
        assert_eq!(*decoded.get("localStopStrings"), Msg::Undefined);
        assert!(matches!(&decoded, Msg::Map(entries) if entries.iter().any(|(key, _)| key == "localStopStrings")));
        assert_eq!(decoded.to_json(), synthetic_preset_json("test-module"));

        let module = synthetic_module("test-module");
        assert_eq!(decode_risu_module(&risu_module_file(&module)).unwrap(), module);
    }

    #[test]
    fn 리스_파일이_아니면_해독_오류를_낸다() {
        assert_eq!(decode_risu_preset(b"not a preset").unwrap_err().0, "리스 프리셋(.risup) 파일이 아닙니다.");
        assert_eq!(decode_risu_module(b"not a module").unwrap_err().0, "리스 모듈(.risum) 파일이 아닙니다.");
        let tampered = risu_preset_file(&synthetic_preset("test-module"));
        assert!(decode_risu_preset(&tampered[..tampered.len() - 3]).is_err());
        assert_eq!(decode_risu_preset(b"").unwrap_err().0, "리스 프리셋(.risup) 파일이 아닙니다.");
    }

    #[test]
    fn 프리셋_바깥과_본문_오류를_node_문구로_나눈다() {
        let outer = |entries: Vec<(&str, Msg)>| {
            let map = Msg::Map(entries.into_iter().map(|(key, value)| (key.to_string(), value)).collect());
            encode_rpack(&gzip(&fixtures::encode_msgpack(&map)))
        };
        let preset = |value: Msg| outer(vec![("type", Msg::String("preset".into())), ("presetVersion", Msg::Number(0.0)), ("pres", value)]);
        assert_eq!(decode_risu_preset(&outer(vec![("type", Msg::String("preset".into()))])).unwrap_err().0, "지원하지 않는 리스 프리셋 버전입니다.");
        assert_eq!(decode_risu_preset(&preset(Msg::Null)).unwrap_err().0, "프리셋 본문이 없습니다.");
        assert_eq!(decode_risu_preset(&preset(Msg::Binary(vec![1; 8]))).unwrap_err().0, "암호화된 프리셋이 너무 짧습니다.");
        assert_eq!(decode_risu_preset(&preset(Msg::Binary(vec![1; 32]))).unwrap_err().0, "프리셋 본문을 풀지 못했습니다.");
        assert_eq!(decode_risu_preset(&preset(Msg::Number(5.0))).unwrap_err().0, "프리셋 본문을 풀지 못했습니다.");
        let mut extra = fixtures::encode_msgpack(&Msg::Null);
        extra.push(0);
        assert_eq!(decode_risu_preset(&encode_rpack(&gzip(&extra))).unwrap_err().0, "msgpack 뒤에 남은 데이터가 있습니다.");
        assert_eq!(decode_risu_preset(&encode_rpack(&gzip(&[0xc1]))).unwrap_err().0, "지원하지 않는 msgpack 타입입니다: 0xc1");
        assert_eq!(decode_risu_preset(&encode_rpack(&gzip(&[0xd4, 1, 0]))).unwrap_err().0, "지원하지 않는 msgpack 확장 타입입니다.");
        assert_eq!(decode_risu_preset(&encode_rpack(&gzip(&[0xdb, 0, 0, 0, 9]))).unwrap_err().0, "msgpack 데이터가 잘렸습니다.");
    }

    #[test]
    fn 모듈_머리와_본문_오류를_node_문구로_나눈다() {
        let mut short = vec![111, 0];
        short.extend(10_u32.to_le_bytes());
        assert_eq!(decode_risu_module(&short).unwrap_err().0, "모듈 데이터가 잘렸습니다.");
        let body = encode_rpack(b"{not json");
        let mut broken = vec![111, 0];
        broken.extend((body.len() as u32).to_le_bytes());
        broken.extend(body);
        assert_eq!(decode_risu_module(&broken).unwrap_err().0, "모듈 본문을 풀지 못했습니다.");
        let wrong = risu_module_file(&json!(0));
        assert_eq!(decode_risu_module(&wrong).unwrap_err().0, "리스 모듈 형식이 아닙니다.");
    }

    #[test]
    fn msgpack_값을_js처럼_읽고_json으로_바꾼다() {
        let map = [0x83, 0xa1, b'a', 0x01, 0xa1, b'b', 0xd4, 0, 0, 0xa1, b'a', 0xff];
        let value = decode_msgpack(&map).unwrap();
        assert_eq!(value, Msg::Map(vec![("a".into(), Msg::Number(-1.0)), ("b".into(), Msg::Undefined)]));
        assert_eq!(value.to_json(), json!({ "a": -1 }));
        let keys = [0x82, 0x05, 0xc3, 0xc0, 0xc2];
        assert_eq!(decode_msgpack(&keys).unwrap().to_json(), json!({ "5": true, "null": false }));
        assert_eq!(decode_msgpack(&[0x92, 0xd4, 0, 0, 0xc4, 1, 7]).unwrap().to_json(), json!([null, { "type": "Buffer", "data": [7] }]));
        assert_eq!(decode_msgpack(&[0xca, 0x3f, 0xc0, 0, 0]).unwrap().to_json(), json!(1.5));
    }
}
