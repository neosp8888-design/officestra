// Antigravity가 로컬 SQLite에 남긴 대화 메타데이터를 읽기 전용으로 해석한다. Node의 antigravity-local-state.mjs를 옮겼다.

use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::sync::{Arc, LazyLock, Mutex};

use rusqlite::types::ValueRef;
use rusqlite::{Connection, OpenFlags, Row};
use serde::Serialize;
use serde_json::{Value, json};

use crate::js::{is_js_integer, iso_string, join_path, js_trim, string_to_number};
use crate::local_artifacts::{home_directory, modified_milliseconds};
use crate::session_context_usage::{ContextEntry, ContextUsage};

const MAX_METADATA_BYTES: i64 = 64 * 1024;

#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TokenUsage {
    pub input_tokens: f64,
    pub output_tokens: f64,
    pub cached_input_tokens: f64,
    pub reasoning_output_tokens: f64,
}

impl TokenUsage {
    fn add(&mut self, source: &TokenUsage) {
        self.input_tokens += source.input_tokens;
        self.output_tokens += source.output_tokens;
        self.cached_input_tokens += source.cached_input_tokens;
        self.reasoning_output_tokens += source.reasoning_output_tokens;
    }
}

#[derive(Debug, PartialEq)]
pub struct GeneratorMetadata {
    pub execution_id: String,
    pub used_tokens: f64,
    pub limit_tokens: f64,
}

#[derive(Debug, PartialEq)]
pub struct StepMetadata {
    pub execution_id: String,
    pub at: f64,
    pub usage: TokenUsage,
}

struct ParsedState {
    context_entries: Vec<ContextEntry>,
    usage: Option<TokenUsage>,
}

struct CachedState {
    signature: String,
    value: Arc<ParsedState>,
}

static PARSED_STATE_CACHE: LazyLock<Mutex<HashMap<String, CachedState>>> = LazyLock::new(Mutex::default);

pub fn default_conversation_root() -> String {
    join_path(&[&home_directory(), ".gemini", "antigravity-cli", "conversations"])
}

pub(crate) fn is_session_id(id: &str) -> bool {
    let bytes = id.as_bytes();
    bytes.len() == 36
        && bytes.iter().enumerate().all(|(index, byte)| match index {
            8 | 13 | 18 | 23 => *byte == b'-',
            _ => byte.is_ascii_hexdigit(),
        })
}

pub fn antigravity_conversation_path(session_id: &str, root: &str) -> Option<String> {
    let id = js_trim(session_id);
    is_session_id(id).then(|| join_path(&[root, &format!("{id}.db")]))
}

pub fn antigravity_context_usage(session_id: &str, at: f64, root: &str) -> Option<ContextUsage> {
    if !at.is_finite() {
        return None;
    }
    let state = read_state(&antigravity_conversation_path(session_id, root)?)?;
    let matched = state.context_entries.iter().take_while(|entry| entry.at <= at).last()?;
    let limit_tokens = matched.limit_tokens.filter(|limit| *limit > 0.0)?;
    Some(ContextUsage { used_tokens: matched.used_tokens.min(limit_tokens), limit_tokens })
}

/// 중단 복구용 누적 사용량. Node의 usagePayload처럼 비용 관련 칸은 null로 채운다.
pub fn antigravity_session_usage(session_id: &str, root: &str) -> Option<Value> {
    let usage = read_state(&antigravity_conversation_path(session_id, root)?)?.usage?;
    Some(json!({
        "inputTokens": usage.input_tokens,
        "outputTokens": usage.output_tokens,
        "cachedInputTokens": usage.cached_input_tokens,
        "reasoningOutputTokens": usage.reasoning_output_tokens,
        "cacheWriteInputTokens": null,
        "cacheWrite5mInputTokens": null,
        "cacheWrite1hInputTokens": null,
        "serviceTier": null,
        "speed": null,
        "inferenceGeo": null,
        "reportedCostUsd": null,
    }))
}

/// 완료된 모델 단계마다 입력·캐시 사용량을 따로 남긴다. 세션 차이 대신 단계 경계를 지켜 문맥 길이별 단가를 매긴다.
/// after_index가 정수면 그 뒤 단계만, 아니면 started_at 이후 단계만 고르고 ended_at 뒤 단계는 뺀다.
/// 시각은 Date의 getTime 밀리초이고, 단계 하나라도 해석하지 못하면 원장 전체를 버린다.
pub fn antigravity_turn_request_usages(
    session_id: &str,
    root: &str,
    after_index: Option<f64>,
    started_at: Option<f64>,
    ended_at: f64,
) -> Option<Vec<Value>> {
    let path = antigravity_conversation_path(session_id, root)?;
    if !Path::new(&path).exists() || !ended_at.is_finite() {
        return None;
    }
    let after_index = after_index.filter(|index| is_js_integer(*index));
    let from = started_at.unwrap_or(f64::NAN);
    if after_index.is_none() && !from.is_finite() {
        return None;
    }
    let mut requests = Vec::new();
    for metadata in turn_request_rows(&path, after_index.unwrap_or(-1.0)).ok()? {
        let parsed = parse_step_metadata(&metadata?)?;
        if (after_index.is_none() && parsed.at < from) || parsed.at > ended_at {
            continue;
        }
        requests.push(json!({ "usage": parsed.usage, "pricedAt": iso_string(parsed.at) }));
    }
    Some(requests)
}

fn turn_request_rows(path: &str, after_index: f64) -> rusqlite::Result<MetadataRows> {
    let database = open_conversation(path)?;
    let mut statement = database.prepare(
        "SELECT idx, metadata FROM steps WHERE step_type = 15 AND idx > ? AND metadata IS NOT NULL AND length(metadata) <= ? ORDER BY idx",
    )?;
    // node:sqlite는 JS 숫자를 REAL로 묶는다.
    let rows = statement.query_map((after_index, MAX_METADATA_BYTES as f64), |row| {
        sqlite_value(row, 0)?;
        Ok(sqlite_value(row, 1)?.buffer())
    })?;
    rows.collect()
}

pub(crate) fn open_conversation(path: &str) -> rusqlite::Result<Connection> {
    Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX)
}

/// node:sqlite가 돌려주는 JS 값. 안전 정수 범위를 넘는 INTEGER는 행을 읽는 순간 ERR_OUT_OF_RANGE로 던진다.
pub(crate) enum SqliteValue {
    Null,
    Number(f64),
    Text(String),
    Blob(Vec<u8>),
}

pub(crate) fn sqlite_value(row: &Row, index: usize) -> rusqlite::Result<SqliteValue> {
    const MAX_SAFE_INTEGER: u64 = (1 << 53) - 1;
    Ok(match row.get_ref(index)? {
        ValueRef::Null => SqliteValue::Null,
        ValueRef::Integer(value) if value.unsigned_abs() > MAX_SAFE_INTEGER => {
            return Err(rusqlite::Error::IntegralValueOutOfRange(index, value));
        }
        ValueRef::Integer(value) => SqliteValue::Number(value as f64),
        ValueRef::Real(value) => SqliteValue::Number(value),
        ValueRef::Text(text) => SqliteValue::Text(String::from_utf8_lossy(text).into_owned()),
        ValueRef::Blob(bytes) => SqliteValue::Blob(bytes.to_vec()),
    })
}

impl SqliteValue {
    /// protobufFields의 `Buffer.from(value ?? [])`. 문자열은 UTF-8 바이트가 되고 숫자는 TypeError라 해석 실패(None)다.
    pub(crate) fn buffer(self) -> Option<Vec<u8>> {
        match self {
            SqliteValue::Null => Some(Vec::new()),
            SqliteValue::Number(_) => None,
            SqliteValue::Text(text) => Some(text.into_bytes()),
            SqliteValue::Blob(bytes) => Some(bytes),
        }
    }

    /// `Number(value)`. Uint8Array는 쉼표로 이은 문자열을 거친다.
    pub(crate) fn number(&self) -> f64 {
        match self {
            SqliteValue::Null => 0.0,
            SqliteValue::Number(value) => *value,
            SqliteValue::Text(text) => string_to_number(text),
            SqliteValue::Blob(bytes) => {
                string_to_number(&bytes.iter().map(u8::to_string).collect::<Vec<_>>().join(","))
            }
        }
    }
}

fn read_state(path: &str) -> Option<Arc<ParsedState>> {
    if !Path::new(path).exists() {
        return None;
    }
    let signature = file_signature(path);
    let cached = PARSED_STATE_CACHE
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .get(path)
        .map(|cached| (cached.signature.clone(), cached.value.clone()));
    if let Some((cached_signature, value)) = &cached
        && *cached_signature == signature
    {
        return Some(value.clone());
    }
    match read_rows(path) {
        Ok((generator_rows, step_rows)) => {
            let value = Arc::new(parsed_state(&generator_rows, &step_rows));
            PARSED_STATE_CACHE
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .insert(path.to_string(), CachedState { signature, value: value.clone() });
            Some(value)
        }
        // 대화가 동시에 갱신되는 순간의 읽기 오류는 직전 해석값으로 버틴다.
        Err(_) => cached.map(|(_, value)| value),
    }
}

type MetadataRows = Vec<Option<Vec<u8>>>;

fn read_rows(path: &str) -> rusqlite::Result<(MetadataRows, MetadataRows)> {
    let database = open_conversation(path)?;
    let generator_rows = metadata_rows(&database, "SELECT data FROM gen_metadata WHERE length(data) <= ? ORDER BY idx")?;
    let step_rows = metadata_rows(
        &database,
        "SELECT metadata FROM steps WHERE step_type = 15 AND metadata IS NOT NULL AND length(metadata) <= ? ORDER BY idx",
    )?;
    Ok((generator_rows, step_rows))
}

/// node:sqlite는 BLOB을 바이트로, TEXT를 문자열로 돌려주고 Buffer.from이 문자열을 UTF-8로 바꾼다.
/// 정수·실수는 Buffer.from이 실패해 해석 실패(None)가 되고, 안전 범위를 넘는 정수는 읽기 전체가 실패한다.
fn metadata_rows(database: &Connection, sql: &str) -> rusqlite::Result<MetadataRows> {
    let mut statement = database.prepare(sql)?;
    let rows = statement.query_map([MAX_METADATA_BYTES], |row| Ok(sqlite_value(row, 0)?.buffer()))?;
    rows.collect()
}

fn parsed_state(generator_rows: &[Option<Vec<u8>>], step_rows: &[Option<Vec<u8>>]) -> ParsedState {
    // JS Map은 같은 키를 다시 넣어도 처음 들어간 자리를 지킨다.
    let mut context_order: Vec<String> = Vec::new();
    let mut contexts: HashMap<String, GeneratorMetadata> = HashMap::new();
    for parsed in generator_rows.iter().filter_map(|row| parse_generator_metadata(row.as_deref()?)) {
        if !contexts.contains_key(&parsed.execution_id) {
            context_order.push(parsed.execution_id.clone());
        }
        contexts.insert(parsed.execution_id.clone(), parsed);
    }

    let mut executions: HashMap<String, (f64, TokenUsage)> = HashMap::new();
    let mut total = TokenUsage::default();
    let mut usage_count = 0;
    for parsed in step_rows.iter().filter_map(|row| parse_step_metadata(row.as_deref()?)) {
        let execution = executions.entry(parsed.execution_id).or_insert((parsed.at, TokenUsage::default()));
        execution.0 = execution.0.max(parsed.at);
        execution.1.add(&parsed.usage);
        total.add(&parsed.usage);
        usage_count += 1;
    }

    let mut context_entries: Vec<ContextEntry> = context_order
        .iter()
        .filter_map(|execution_id| {
            let context = &contexts[execution_id];
            let (at, _) = executions.get(execution_id)?;
            Some(ContextEntry { at: *at, used_tokens: context.used_tokens, limit_tokens: Some(context.limit_tokens) })
        })
        .collect();
    context_entries.sort_by(|left, right| left.at.total_cmp(&right.at));
    ParsedState { context_entries, usage: (usage_count > 0).then_some(total) }
}

pub fn parse_generator_metadata(value: &[u8]) -> Option<GeneratorMetadata> {
    let fields = protobuf_fields(value).ok()?;
    let execution_id = utf8_field(&fields, 4);
    let model = nested_fields(&fields, 1).ok()?;
    let start = nested_fields(&model, 9).ok()?;
    let context = nested_fields(&start, 10).ok()?;
    let used_tokens = number_field(&context, 1);
    let limit_tokens = number_field(&context, 4);
    if execution_id.is_empty() || used_tokens <= 0.0 || limit_tokens <= 0.0 {
        return None;
    }
    Some(GeneratorMetadata { execution_id, used_tokens, limit_tokens })
}

pub fn parse_step_metadata(value: &[u8]) -> Option<StepMetadata> {
    let fields = protobuf_fields(value).ok()?;
    let execution_id = utf8_field(&fields, 12);
    let timestamp = nested_fields(&fields, 1).ok()?;
    let seconds = number_field(&timestamp, 1);
    let nanos = number_field(&timestamp, 2);
    let usage = nested_fields(&fields, 9).ok()?;
    if execution_id.is_empty() || seconds <= 0.0 || usage.is_empty() {
        return None;
    }
    Some(StepMetadata {
        execution_id,
        at: seconds * 1_000.0 + (nanos / 1_000_000.0).trunc(),
        usage: TokenUsage {
            input_tokens: number_field(&usage, 2),
            output_tokens: number_field(&usage, 3),
            cached_input_tokens: number_field(&usage, 5),
            reasoning_output_tokens: number_field(&usage, 9),
        },
    })
}

fn file_signature(path: &str) -> String {
    [path.to_string(), format!("{path}-wal")]
        .iter()
        .map(|candidate| match fs::metadata(candidate) {
            Ok(metadata) => format!("{}:{}", metadata.len(), modified_milliseconds(&metadata)),
            Err(_) => "missing".into(),
        })
        .collect::<Vec<_>>()
        .join("|")
}

#[derive(Debug)]
pub enum FieldValue<'a> {
    /// varint 값. Node는 BigInt를 Number로 바꾸므로 가장 가까운 f64로 둔다.
    Number(f64),
    Bytes(&'a [u8]),
}

#[derive(Debug)]
pub struct Field<'a> {
    pub number: u128,
    pub value: FieldValue<'a>,
}

#[derive(Debug, PartialEq)]
pub struct ProtobufError;

pub fn nested_fields<'a>(fields: &[Field<'a>], number: u128) -> Result<Vec<Field<'a>>, ProtobufError> {
    match bytes_field(fields, number) {
        Some(value) => protobuf_fields(value),
        None => Ok(Vec::new()),
    }
}

fn bytes_field<'a>(fields: &[Field<'a>], number: u128) -> Option<&'a [u8]> {
    fields.iter().find_map(|field| match field.value {
        FieldValue::Bytes(value) if field.number == number => Some(value),
        _ => None,
    })
}

pub fn utf8_field(fields: &[Field], number: u128) -> String {
    bytes_field(fields, number).map_or_else(String::new, |value| js_trim(&String::from_utf8_lossy(value)).to_string())
}

pub fn number_field(fields: &[Field], number: u128) -> f64 {
    fields
        .iter()
        .find_map(|field| match field.value {
            FieldValue::Number(value) if field.number == number => Some(value),
            _ => None,
        })
        .unwrap_or(0.0)
}

pub fn protobuf_fields(buffer: &[u8]) -> Result<Vec<Field<'_>>, ProtobufError> {
    let mut fields = Vec::new();
    let mut offset = 0;
    while offset < buffer.len() {
        let (tag, next) = read_varint(buffer, offset)?;
        offset = next;
        let number = tag >> 3;
        if number == 0 {
            return Err(ProtobufError);
        }
        match tag & 7 {
            0 => {
                let (value, next) = read_varint(buffer, offset)?;
                offset = next;
                fields.push(Field { number, value: FieldValue::Number(value as f64) });
            }
            1 => offset = checked_offset(buffer, offset, 8_u8)?,
            2 => {
                let (length, next) = read_varint(buffer, offset)?;
                offset = next;
                let end = checked_offset(buffer, offset, length)?;
                fields.push(Field { number, value: FieldValue::Bytes(&buffer[offset..end]) });
                offset = end;
            }
            5 => offset = checked_offset(buffer, offset, 4_u8)?,
            _ => return Err(ProtobufError),
        }
    }
    Ok(fields)
}

/// Node와 같이 7비트 조각을 70비트 자리까지만 읽는다(최대 11바이트).
fn read_varint(buffer: &[u8], start: usize) -> Result<(u128, usize), ProtobufError> {
    let mut value: u128 = 0;
    let mut shift = 0;
    let mut offset = start;
    while offset < buffer.len() && shift <= 70 {
        let byte = buffer[offset];
        offset += 1;
        value |= u128::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Ok((value, offset));
        }
        shift += 7;
    }
    Err(ProtobufError)
}

fn checked_offset(buffer: &[u8], offset: usize, length: impl Into<u128>) -> Result<usize, ProtobufError> {
    let length: u128 = length.into();
    if length > (buffer.len() - offset) as u128 {
        return Err(ProtobufError);
    }
    Ok(offset + length as usize)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SESSION_ID: &str = "11111111-2222-4333-8444-555555555555";

    fn varint(mut value: u128) -> Vec<u8> {
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

    fn number(field: u128, value: u128) -> Vec<u8> {
        [varint(field << 3), varint(value)].concat()
    }

    fn bytes(field: u128, value: &[u8]) -> Vec<u8> {
        [varint((field << 3) | 2), varint(value.len() as u128), value.to_vec()].concat()
    }

    fn generator_metadata(execution_id: &str, used: u128, limit: u128) -> Vec<u8> {
        let context = [number(1, used), number(4, limit)].concat();
        [bytes(1, &bytes(9, &bytes(10, &context))), bytes(4, execution_id.as_bytes())].concat()
    }

    fn step_metadata(execution_id: &str, seconds: u128, input: u128, output: u128, cached: u128, reasoning: u128) -> Vec<u8> {
        let usage = [number(2, input), number(3, output), number(5, cached), number(9, reasoning)].concat();
        [bytes(1, &number(1, seconds)), bytes(9, &usage), bytes(12, execution_id.as_bytes())].concat()
    }

    #[test]
    fn protobuf_메타데이터에서_실제_컨텍스트_값을_읽는다() {
        assert_eq!(
            parse_generator_metadata(&generator_metadata("execution-1", 58_559, 256_000)),
            Some(GeneratorMetadata { execution_id: "execution-1".into(), used_tokens: 58_559.0, limit_tokens: 256_000.0 })
        );
    }

    #[test]
    fn 응답_단계에서_토큰과_시각을_읽는다() {
        assert_eq!(
            parse_step_metadata(&step_metadata("execution-1", 1_800_000_000, 1_200, 30, 900, 12)),
            Some(StepMetadata {
                execution_id: "execution-1".into(),
                at: 1_800_000_000_000.0,
                usage: TokenUsage {
                    input_tokens: 1_200.0,
                    output_tokens: 30.0,
                    cached_input_tokens: 900.0,
                    reasoning_output_tokens: 12.0,
                },
            })
        );
    }

    #[test]
    fn 깨진_protobuf는_해석하지_않는다() {
        // 길이가 버퍼를 넘는 바이트 칸, 지원하지 않는 wire 3, 필드 번호 0.
        assert_eq!(parse_step_metadata(&[0x0a, 0x05, 0x01]), None);
        assert_eq!(protobuf_fields(&[0x0b]).unwrap_err(), ProtobufError);
        assert_eq!(protobuf_fields(&[0x00, 0x01]).unwrap_err(), ProtobufError);
        // 70비트 자리를 넘는 varint.
        assert_eq!(protobuf_fields(&[0x08, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x01]).unwrap_err(), ProtobufError);
    }

    #[test]
    fn 대화_db는_시점별_컨텍스트와_중단_복구용_누적_사용량을_제공한다() {
        let root = std::env::temp_dir().join(format!("officestra-agy-state-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        let root = root.to_string_lossy().into_owned();
        let path = join_path(&[&root, &format!("{SESSION_ID}.db")]);
        {
            let database = Connection::open(&path).unwrap();
            database
                .execute_batch(
                    "CREATE TABLE gen_metadata (idx INTEGER PRIMARY KEY, data BLOB);
                     CREATE TABLE steps (idx INTEGER PRIMARY KEY, step_type INTEGER, metadata BLOB);",
                )
                .unwrap();
            let generator = "INSERT INTO gen_metadata (idx, data) VALUES (?, ?)";
            let step = "INSERT INTO steps (idx, step_type, metadata) VALUES (?, 15, ?)";
            database.execute(generator, (1, generator_metadata("execution-1", 50, 200))).unwrap();
            database.execute(step, (1, step_metadata("execution-1", 1_000, 10, 2, 30, 1))).unwrap();
            database.execute(generator, (2, generator_metadata("execution-2", 100, 200))).unwrap();
            database.execute(step, (2, step_metadata("execution-2", 2_000, 20, 3, 40, 2))).unwrap();
        }
        let context = antigravity_context_usage(SESSION_ID, 1_500_000.0, &root);
        let usage = antigravity_session_usage(SESSION_ID, &root);
        fs::remove_dir_all(&root).unwrap();

        assert_eq!(context, Some(ContextUsage { used_tokens: 50.0, limit_tokens: 200.0 }));
        assert_eq!(
            String::from_utf8(crate::js::to_js_json(&usage.unwrap()).unwrap()).unwrap(),
            r#"{"inputTokens":30,"outputTokens":5,"cachedInputTokens":70,"reasoningOutputTokens":3,"cacheWriteInputTokens":null,"cacheWrite5mInputTokens":null,"cacheWrite1hInputTokens":null,"serviceTier":null,"speed":null,"inferenceGeo":null,"reportedCostUsd":null}"#
        );
    }

    fn temporary_root(name: &str) -> String {
        let root = std::env::temp_dir().join(format!("officestra-agy-{name}-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        root.to_string_lossy().into_owned()
    }

    #[test]
    fn 턴_요청_원장을_sqlite_인덱스_또는_시각으로_분리한다() {
        let root = temporary_root("requests");
        {
            let database = Connection::open(join_path(&[&root, &format!("{SESSION_ID}.db")])).unwrap();
            database.execute_batch("CREATE TABLE steps (idx INTEGER PRIMARY KEY, step_type INTEGER, metadata BLOB)").unwrap();
            for idx in [1, 2, 3] {
                let metadata = step_metadata(&format!("request-{idx}"), idx * 1000, 100_000, 100, 50_000, 10);
                database.execute("INSERT INTO steps VALUES (?, 15, ?)", (idx as i64, metadata)).unwrap();
            }
        }
        let by_index = antigravity_turn_request_usages(SESSION_ID, &root, Some(1.0), None, 3_000_000.0);
        let by_time = antigravity_turn_request_usages(SESSION_ID, &root, None, Some(2_000_000.0), 3_000_000.0);
        // 정수가 아닌 기준 단계는 없는 것으로 보고 시각으로 고른다.
        let fractional_index = antigravity_turn_request_usages(SESSION_ID, &root, Some(1.5), Some(2_000_000.0), 3_000_000.0);
        let unbounded = antigravity_turn_request_usages(SESSION_ID, &root, None, None, 3_000_000.0);
        let invalid_end = antigravity_turn_request_usages(SESSION_ID, &root, Some(1.0), None, f64::NAN);
        fs::remove_dir_all(&root).unwrap();

        assert_eq!(by_index, by_time);
        assert_eq!(by_index, fractional_index);
        let usage = r#"{"inputTokens":100000,"outputTokens":100,"cachedInputTokens":50000,"reasoningOutputTokens":10}"#;
        assert_eq!(
            String::from_utf8(crate::js::to_js_json(&by_index.unwrap()).unwrap()).unwrap(),
            format!(r#"[{{"usage":{usage},"pricedAt":"1970-01-01T00:33:20.000Z"}},{{"usage":{usage},"pricedAt":"1970-01-01T00:50:00.000Z"}}]"#)
        );
        assert_eq!(unbounded, None);
        assert_eq!(invalid_end, None);
    }

    // node:sqlite는 숫자 metadata에서 Buffer.from이 던지고, 안전 범위를 넘는 정수는 행 읽기에서 던진다. 둘 다 원장을 버린다.
    #[test]
    fn 해석할_수_없는_단계가_있으면_턴_요청_원장을_만들지_않는다() {
        let root = temporary_root("requests-invalid");
        let cases = [
            ("11111111-2222-4333-8444-000000000001", "INSERT INTO steps VALUES (2, 15, 12)"),
            ("11111111-2222-4333-8444-000000000002", "INSERT INTO steps VALUES (9007199254740993, 15, X'00')"),
        ];
        let mut results = Vec::new();
        for (session_id, insert) in cases {
            {
                let database = Connection::open(join_path(&[&root, &format!("{session_id}.db")])).unwrap();
                database.execute_batch("CREATE TABLE steps (idx, step_type, metadata)").unwrap();
                database.execute("INSERT INTO steps VALUES (1, 15, ?)", [step_metadata("request-1", 1_000, 1, 1, 1, 1)]).unwrap();
                database.execute_batch(insert).unwrap();
            }
            results.push(antigravity_turn_request_usages(session_id, &root, Some(0.0), None, 3_000_000.0));
        }
        fs::remove_dir_all(&root).unwrap();
        assert_eq!(results, [None, None]);
    }

    #[test]
    fn 세션_id가_아니면_경로를_만들지_않는다() {
        assert_eq!(antigravity_conversation_path(" 11111111-2222-4333-8444-555555555555 ", "/r").as_deref(), Some("/r/11111111-2222-4333-8444-555555555555.db"));
        assert_eq!(antigravity_conversation_path("11111111-2222-4333-8444-55555555555G", "/r"), None);
        assert_eq!(antigravity_conversation_path("../x", "/r"), None);
    }
}
