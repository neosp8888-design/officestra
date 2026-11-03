// 롤플레이 대화의 프리셋 가져오기, 직원별 설정, 대화 기록, 모델 호출을 맡는다. Node roleplay-service.mjs 이식.

use std::cmp::Ordering;
use std::collections::HashMap;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::process::Stdio;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use serde::Serialize;
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use sqlx::postgres::PgRow;
use sqlx::{PgPool, Row};
use tokio::sync::watch;

use super::cbs::CbsContext;
use super::js_sort::js_sort_by;
use super::prompt::{
    AssembleInput, JsError, ToggleDefinition, active_modules, apply_regex_scripts, assemble_roleplay_messages, fit_roleplay_context,
    parse_toggle_definitions, roleplay_request_body,
};
use super::risu_format::{Msg, decode_risu_module, decode_risu_preset};
use super::{Asset, ChatMessage, js_number};
use crate::js::{
    is_js_integer, join_path, js_compare, js_keys, js_length, js_min, js_string, js_to_number, js_trim, js_truthy, number_to_string,
    parse_js_json, to_js_json, utf16_prefix,
};
use crate::local_provider::service::LocalProviderService;
use crate::local_provider::{LocalDefinition, normalize_local_definition};
use crate::rag::database_message;
use crate::usage_summary::base64url_decode;

const MAX_IMPORT_BYTES: usize = 20 * 1024 * 1024;
const MAX_TEXT: usize = 200_000;
const MAX_MESSAGE: usize = 20_000;
const REQUEST_TIMEOUT: Duration = Duration::from_millis(300_000);
// 프록시는 목록을 계정 서버에서 받아 오므로 몇 초 걸릴 수 있다.
const MODEL_LIST_TIMEOUT: Duration = Duration::from_millis(15_000);
// 로컬은 4090에 모델을 올리고 앞선 요청을 기다리는 시간까지 포함한다.
const LOCAL_REQUEST_TIMEOUT: Duration = Duration::from_millis(600_000);
// count_tokens에 안 잡히는 생성 시작 표시 몇 토큰을 위한 여유다.
const CONTEXT_MARGIN_TOKENS: f64 = 256.0;
const DEFAULT_API_URL: &str = "http://127.0.0.1:8080/v1/messages";
const NOT_GENERATED: &str = "답을 만들지 못했습니다.";
const KEY_FILE_UNREADABLE: &str = "롤플레이 모델 접근 키 파일을 읽지 못했습니다.";
const PROXY_UNREACHABLE: &str = "롤플레이 모델 서버에 연결하지 못했습니다. 프록시가 켜져 있는지 확인하세요.";
// 기본 주소의 프록시(맥 안 안티그래비티 프록시)가 꺼져 있으면 이 스크립트로 켠다.
const PROXY_ADDRESS: &str = "127.0.0.1:8080";
// 켜기 스크립트(cli start)는 서버가 뜰 때까지 최대 11.5초 기다린 뒤 끝난다.
const PROXY_START_TIMEOUT: Duration = Duration::from_secs(15);
// 스크립트가 끝난 뒤 포트가 열리길 더 기다리는 시간이다.
const PROXY_PORT_WAIT: Duration = Duration::from_secs(5);

/// Node Date를 JSON으로 보낼 때의 ISO 문자열(밀리초, UTC).
macro_rules! iso {
    ($column:literal) => {
        concat!("to_char(", $column, " AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z\"')")
    };
}

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// 서비스 오류. Status는 Node RoleplayError, Internal은 그 밖의 JS 오류(HTTP 500, 뒤에서 만들던 답은 "답을 만들지 못했습니다.")다.
#[derive(Debug, Clone, PartialEq)]
pub enum RoleplayError {
    Status(u16, String),
    Internal(String),
}

impl RoleplayError {
    fn new(status: u16, message: impl Into<String>) -> Self {
        Self::Status(status, message.into())
    }

    pub fn status(&self) -> u16 {
        match self {
            Self::Status(status, _) => *status,
            Self::Internal(_) => 500,
        }
    }

    pub fn message(&self) -> &str {
        match self {
            Self::Status(_, message) | Self::Internal(message) => message,
        }
    }
}

impl From<sqlx::Error> for RoleplayError {
    fn from(error: sqlx::Error) -> Self {
        Self::Internal(database_message(&error))
    }
}

impl From<JsError> for RoleplayError {
    fn from(error: JsError) -> Self {
        Self::Internal(error.0)
    }
}

/// fetch 요청. 머리글 이름은 Node와 같은 소문자다.
#[derive(Debug, Clone, PartialEq)]
pub struct FetchRequest {
    pub method: &'static str,
    pub url: String,
    pub headers: Vec<(&'static str, String)>,
    pub body: Option<String>,
    pub timeout: Duration,
}

/// 받은 응답. body는 `response.text()`이고 읽다 실패하면 Err다(Node에서는 잡지 않는 오류).
#[derive(Debug, Clone, PartialEq)]
pub struct FetchResponse {
    pub status: u16,
    pub body: Result<String, String>,
}

impl FetchResponse {
    pub fn ok(&self) -> bool {
        (200..300).contains(&self.status)
    }
}

/// fetch가 응답 전에 실패한 이유. 시간 초과(TimeoutError)만 문구가 다르다.
/// Unreachable은 연결부터 못 해 요청이 나가지 않은 경우라 다시 보내도 겹치지 않는다.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FetchFailure {
    Timeout,
    Unreachable,
    Failed,
}

pub trait RoleplayFetch: Send + Sync {
    fn fetch(&self, request: FetchRequest) -> BoxFuture<'_, Result<FetchResponse, FetchFailure>>;
}

/// 실제 HTTP fetch. Node fetch처럼 환경 프록시를 쓰지 않고, 시간 제한은 본문을 다 읽을 때까지다.
pub struct HttpFetch {
    client: reqwest::Client,
}

impl HttpFetch {
    pub fn new() -> Result<Self, String> {
        let client = crate::http_client_builder().no_proxy().build().map_err(|error| error.to_string())?;
        Ok(Self { client })
    }
}

impl RoleplayFetch for HttpFetch {
    fn fetch(&self, request: FetchRequest) -> BoxFuture<'_, Result<FetchResponse, FetchFailure>> {
        Box::pin(async move {
            let method = reqwest::Method::from_bytes(request.method.as_bytes()).map_err(|_| FetchFailure::Failed)?;
            let mut builder = self.client.request(method, request.url.as_str()).timeout(request.timeout);
            for (name, value) in &request.headers {
                builder = builder.header(*name, value.as_str());
            }
            if let Some(body) = request.body {
                builder = builder.body(body);
            }
            let failure = |error: reqwest::Error| {
                if error.is_timeout() {
                    FetchFailure::Timeout
                } else if error.is_connect() {
                    FetchFailure::Unreachable
                } else {
                    FetchFailure::Failed
                }
            };
            let response = builder.send().await.map_err(failure)?;
            let status = response.status().as_u16();
            // fetch text()는 UTF-8로 읽고 앞의 BOM을 뺀다.
            let body = response.bytes().await.map_err(|error| error.to_string()).map(|bytes| {
                let bytes = bytes.strip_prefix(b"\xEF\xBB\xBF".as_slice()).unwrap_or(&bytes);
                String::from_utf8_lossy(bytes).into_owned()
            });
            Ok(FetchResponse { status, body })
        })
    }
}

/// 꺼진 프록시를 켠다. true면 지금 연결할 수 있다는 뜻이다.
pub trait ProxyStarter: Send + Sync {
    fn start(&self) -> BoxFuture<'_, bool>;
}

/// 프록시 켜기 스크립트를 돌린다. 스크립트(cli start)는 서버를 따로 띄우고 끝나므로 백엔드가 꺼져도 프록시는 남는다.
pub struct ScriptProxyStarter {
    script: PathBuf,
    address: String,
    start_timeout: Duration,
    port_wait: Duration,
    /// 여러 요청이 함께 실패해도 스크립트는 한 번만 돈다.
    turn: tokio::sync::Mutex<()>,
}

impl ScriptProxyStarter {
    pub fn new(script: PathBuf, address: impl Into<String>, start_timeout: Duration, port_wait: Duration) -> Self {
        Self { script, address: address.into(), start_timeout, port_wait, turn: tokio::sync::Mutex::new(()) }
    }

    async fn listening(&self) -> bool {
        let connect = tokio::net::TcpStream::connect(self.address.as_str());
        matches!(tokio::time::timeout(Duration::from_secs(1), connect).await, Ok(Ok(_)))
    }
}

impl ProxyStarter for ScriptProxyStarter {
    fn start(&self) -> BoxFuture<'_, bool> {
        Box::pin(async move {
            let _turn = self.turn.lock().await;
            // 기다리는 사이 다른 요청이 이미 켰으면 그대로 쓴다.
            if self.listening().await {
                return true;
            }
            if !self.script.is_file() {
                return false;
            }
            let mut command = tokio::process::Command::new(&self.script);
            if let Some(directory) = self.script.parent() {
                command.current_dir(directory);
            }
            command.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).kill_on_drop(true);
            match tokio::time::timeout(self.start_timeout, command.status()).await {
                Ok(Ok(status)) if status.success() => {}
                Ok(Ok(status)) => {
                    eprintln!("롤플레이 프록시를 켜지 못했습니다({status}).");
                    return false;
                }
                Ok(Err(error)) => {
                    eprintln!("롤플레이 프록시를 켜지 못했습니다: {error}");
                    return false;
                }
                Err(_) => {
                    eprintln!("롤플레이 프록시 켜기 스크립트가 {}초 안에 끝나지 않았습니다.", self.start_timeout.as_secs());
                    return false;
                }
            }
            let deadline = tokio::time::Instant::now() + self.port_wait;
            loop {
                if self.listening().await {
                    eprintln!("롤플레이 프록시가 꺼져 있어 켰습니다.");
                    return true;
                }
                if tokio::time::Instant::now() >= deadline {
                    eprintln!("롤플레이 프록시 켜기 스크립트는 끝났지만 {} 포트가 열리지 않았습니다.", self.address);
                    return false;
                }
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        })
    }
}

/// openMessagesDoor 결과. release는 한 번만 부른다.
pub struct Door {
    pub url: String,
    pub token: String,
    pub model: String,
    /// 없거나 0 이하면 한도가 없는 것으로 본다.
    pub context_window: Option<f64>,
    pub max_output_tokens: f64,
    pub release: Box<dyn FnOnce() -> BoxFuture<'static, Result<(), String>> + Send>,
}

/// Node localProviders의 openMessagesDoor. 오류는 error.message다.
pub trait LocalDoors: Send + Sync {
    fn open_door<'a>(&'a self, definition: &'a Value) -> BoxFuture<'a, Result<Door, String>>;
}

impl LocalDoors for LocalProviderService {
    fn open_door<'a>(&'a self, definition: &'a Value) -> BoxFuture<'a, Result<Door, String>> {
        Box::pin(async move {
            let door = self.open_messages_door(definition).await.map_err(|error| error.message)?;
            let release = door.release;
            Ok(Door {
                url: door.url,
                token: door.token,
                model: door.model,
                context_window: Some(door.context_window as f64),
                max_output_tokens: door.max_output_tokens as f64,
                release: Box::new(move || Box::pin(async move { release.release().await.map_err(|error| error.message) })),
            })
        })
    }
}

pub type LocalDoorsGetter = Arc<dyn Fn() -> Option<Arc<dyn LocalDoors>> + Send + Sync>;

pub struct RoleplayOptions {
    pub pool: PgPool,
    pub broadcast: Arc<dyn Fn(Value) + Send + Sync>,
    pub fetch: Arc<dyn RoleplayFetch>,
    pub api_url: String,
    pub api_key_file: PathBuf,
    /// 서버 시작 순서와 무관하게 호출할 때 꺼내 쓴다.
    pub local_doors: LocalDoorsGetter,
    /// api_url에 연결부터 못 하면 한 번 켜 보고 다시 보낸다. 없으면 켜지 않는다.
    pub proxy_starter: Option<Arc<dyn ProxyStarter>>,
}

impl RoleplayOptions {
    /// OFFICE_ROLEPLAY_API_URL·OFFICE_ROLEPLAY_API_KEY_FILE을 읽는다. 빈 값도 Node `??`처럼 그대로 쓴다.
    pub fn from_env(pool: PgPool, broadcast: Arc<dyn Fn(Value) + Send + Sync>, local_doors: LocalDoorsGetter) -> Result<Self, String> {
        let variable = |name: &str| std::env::var_os(name).map(|value| value.to_string_lossy().into_owned());
        let home = crate::local_artifacts::home_directory();
        let api_key_file = variable("OFFICE_ROLEPLAY_API_KEY_FILE")
            .unwrap_or_else(|| join_path(&[&home, ".config", "antigravity-proxy", "local-api-key"]));
        let api_url = variable("OFFICE_ROLEPLAY_API_URL").unwrap_or_else(|| DEFAULT_API_URL.to_string());
        // 기본 주소(맥 안 프록시)를 쓸 때만 켠다. 다른 주소를 지정했으면 이 프록시를 켜도 소용없다.
        let proxy_starter = (api_url == DEFAULT_API_URL).then(|| {
            let script = PathBuf::from(join_path(&[&home, "antigravity-proxy", "start.sh"]));
            Arc::new(ScriptProxyStarter::new(script, PROXY_ADDRESS, PROXY_START_TIMEOUT, PROXY_PORT_WAIT)) as Arc<dyn ProxyStarter>
        });
        Ok(Self {
            pool,
            broadcast,
            fetch: Arc::new(HttpFetch::new()?),
            api_url,
            api_key_file: PathBuf::from(api_key_file),
            local_doors,
            proxy_starter,
        })
    }
}

#[derive(Default)]
struct State {
    /// 답을 만드는 직원. 값은 끝났을 때 true가 되는 신호다.
    generating: HashMap<String, watch::Receiver<bool>>,
    last_errors: HashMap<String, String>,
}

struct Inner {
    options: RoleplayOptions,
    state: Mutex<State>,
}

#[derive(Clone)]
pub struct RoleplayService(Arc<Inner>);

/// 직원별 롤플레이 설정. 행이 없으면 기본값이다.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Profile {
    pub character_id: String,
    pub enabled: bool,
    pub preset_id: Option<String>,
    pub module_ids: Vec<String>,
    pub toggles: Value,
    pub character_description: String,
    pub first_message: String,
    pub user_name: String,
    pub persona: String,
    pub model: String,
    pub local_profile_id: Option<String>,
    pub speaker_cue: bool,
    pub current_chat_id: Option<String>,
    pub updated_at: Option<String>,
}

impl Profile {
    fn defaults(character_id: &str) -> Self {
        Self {
            character_id: character_id.to_string(),
            enabled: false,
            preset_id: None,
            module_ids: Vec::new(),
            toggles: json!({}),
            character_description: String::new(),
            first_message: String::new(),
            user_name: "사용자".into(),
            persona: String::new(),
            model: "gemini-3.8-flash-high".into(),
            local_profile_id: None,
            speaker_cue: false,
            current_chat_id: None,
            updated_at: None,
        }
    }

    fn from_row(character_id: &str, row: &PgRow) -> Result<Self, sqlx::Error> {
        Ok(Self {
            character_id: character_id.to_string(),
            enabled: row.try_get("enabled")?,
            preset_id: row.try_get("preset_id")?,
            module_ids: row.try_get::<Option<Vec<String>>, _>("module_ids")?.unwrap_or_default(),
            toggles: row.try_get::<Option<Value>, _>("toggles")?.filter(|toggles| !toggles.is_null()).unwrap_or_else(|| json!({})),
            character_description: row.try_get("character_description")?,
            first_message: row.try_get("first_message")?,
            user_name: row.try_get("user_name")?,
            persona: row.try_get("persona")?,
            model: row.try_get("model")?,
            local_profile_id: row.try_get("local_profile_id")?,
            speaker_cue: row.try_get::<Option<bool>, _>("speaker_cue")?.unwrap_or(false),
            current_chat_id: row.try_get("current_chat_id")?,
            updated_at: row.try_get("updated_at")?,
        })
    }
}

struct Character {
    name: String,
}

fn bad_request(message: impl Into<String>) -> RoleplayError {
    RoleplayError::new(400, message)
}

/// Node text(). 길이는 UTF-16 단위다.
fn text<'a>(value: Option<&'a Value>, field: &str, max: usize, required: bool) -> Result<&'a str, RoleplayError> {
    let Some(Value::String(value)) = value else {
        return Err(bad_request(format!("{field}는 문자열이어야 합니다.")));
    };
    if js_length(value) > max {
        return Err(bad_request(format!("{field}가 너무 깁니다.")));
    }
    if required && js_trim(value).is_empty() {
        return Err(bad_request(format!("{field}를 입력하세요.")));
    }
    Ok(value)
}

fn is_uuid(value: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.len() == 36
        && bytes.iter().enumerate().all(|(index, byte)| if matches!(index, 8 | 13 | 18 | 23) { *byte == b'-' } else { byte.is_ascii_hexdigit() })
}

fn uuid(value: &Value, field: &str) -> Result<String, RoleplayError> {
    match value {
        Value::String(text) if is_uuid(text) => Ok(text.clone()),
        _ => Err(bad_request(format!("{field}는 UUID여야 합니다."))),
    }
}

fn toggle_values(value: &Value) -> Result<Map<String, Value>, RoleplayError> {
    let Value::Object(fields) = value else {
        return Err(bad_request("토글 값은 객체여야 합니다."));
    };
    let mut result = Map::new();
    for key in js_keys(fields) {
        if key.is_empty() || js_length(key) > 200 {
            return Err(bad_request("토글 이름이 올바르지 않습니다."));
        }
        let item = text(fields.get(key), &format!("토글 {key}"), 10_000, false)?;
        // `result["__proto__"] = 문자열`은 자기 속성을 만들지 않는다.
        if key != "__proto__" {
            result.insert(key.to_string(), Value::String(item.to_string()));
        }
    }
    Ok(result)
}

fn js_json(value: &impl Serialize) -> String {
    String::from_utf8(to_js_json(value).unwrap_or_else(|_| b"null".to_vec())).unwrap_or_default()
}

fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes).iter().map(|byte| format!("{byte:02x}")).collect()
}

/// ICU 기본 정렬의 ASCII 차례. 글자는 대소문자를 먼저 무시하고, 같으면 소문자가 앞이다.
const ASCII_COLLATION: &str = " _-,;:!?.'\"()[]{}@*/\\&#%`^+<=>|~$0123456789aAbBcCdDeEfFgGhHiIjJkKlLmMnNoOpPqQrRsStTuUvVwWxXyYzZ";

/// `left.localeCompare(right)`. 표에 없는 글자가 있으면 UTF-16 순서로 대신한다(어긋남).
fn locale_compare(left: &str, right: &str) -> Ordering {
    let weights = |text: &str| -> Option<Vec<(usize, bool)>> {
        text.chars()
            .map(|c| ASCII_COLLATION.find(c).map(|index| if c.is_ascii_uppercase() { (index - 1, true) } else { (index, false) }))
            .collect()
    };
    let (Some(left_weights), Some(right_weights)) = (weights(left), weights(right)) else {
        return js_compare(left, right);
    };
    let primary = left_weights.iter().map(|weight| weight.0).cmp(right_weights.iter().map(|weight| weight.0));
    primary.then_with(|| left_weights.iter().map(|weight| weight.1).cmp(right_weights.iter().map(|weight| weight.1)))
}

enum DecodedAsset {
    Module(Value),
    Preset(Msg),
}

/// 확장자로 먼저 고르고, 모르면 모듈·프리셋 순서로 풀어 본다. 반환은 (종류, 이름, JSON 본문)이다.
pub fn decode_roleplay_asset(file_name: &str, buffer: &[u8]) -> Result<(&'static str, String, Value), RoleplayError> {
    let lower = file_name.to_lowercase();
    let attempts: &[&'static str] = if lower.ends_with(".risum") {
        &["module"]
    } else if lower.ends_with(".risup") {
        &["preset"]
    } else {
        &["module", "preset"]
    };
    let mut last_error = None;
    for kind in attempts {
        let decoded = if *kind == "module" {
            decode_risu_module(buffer).map(DecodedAsset::Module)
        } else {
            decode_risu_preset(buffer).map(DecodedAsset::Preset)
        };
        let decoded = match decoded {
            Ok(decoded) => decoded,
            Err(error) => {
                last_error = Some(error.0);
                continue;
            }
        };
        let fallback = lower.strip_suffix(".risum").or_else(|| lower.strip_suffix(".risup")).unwrap_or(&lower);
        let fallback = if fallback.is_empty() { *kind } else { fallback };
        let (name, data) = match decoded {
            DecodedAsset::Module(data) => {
                let name = match &data {
                    Value::Null => return Err(RoleplayError::Internal("Cannot read properties of null (reading 'name')".into())),
                    data => match data.get("name") {
                        None | Some(Value::Null) => String::new(),
                        Some(name) => js_string(name),
                    },
                };
                (name, data)
            }
            DecodedAsset::Preset(data) => {
                let name = match &data {
                    Msg::Null => return Err(RoleplayError::Internal("Cannot read properties of null (reading 'name')".into())),
                    Msg::Undefined => return Err(RoleplayError::Internal("Cannot read properties of undefined (reading 'name')".into())),
                    data => match data.get("name") {
                        Msg::Undefined | Msg::Null => String::new(),
                        name => name.js_string(),
                    },
                };
                (name, data.to_json())
            }
        };
        let name = match js_trim(&name) {
            "" => fallback.to_string(),
            trimmed => trimmed.to_string(),
        };
        return Ok((kind, name, data));
    }
    Err(bad_request(last_error.unwrap_or_else(|| "리스 프리셋이나 모듈 파일이 아닙니다.".into())))
}

fn asset_from_row(row: &PgRow) -> Result<Asset, sqlx::Error> {
    Ok(Asset {
        id: row.try_get("asset_id")?,
        kind: row.try_get("kind")?,
        name: row.try_get("name")?,
        data: row.try_get::<Option<Value>, _>("data")?.unwrap_or(Value::Null),
        imported_at: row.try_get("imported_at_iso")?,
    })
}

const ASSET_COLUMNS: &str = concat!("id::text AS asset_id, kind, name, data, ", iso!("imported_at"), " AS imported_at_iso");

fn asset_summary(asset: &Asset) -> Value {
    let data = &asset.data;
    let template = if asset.kind == "preset" { data.get("customPromptTemplateToggle") } else { data.get("customModuleToggle") };
    let count = |key: &str| data.get(key).and_then(Value::as_array).map_or(0, Vec::len);
    let toggles = parse_toggle_definitions(template)
        .into_iter()
        .filter(|toggle| !matches!(toggle.kind.as_str(), "group" | "groupEnd" | "divider" | "caption"))
        .count();
    json!({
        "id": asset.id,
        "kind": asset.kind,
        "name": asset.name,
        "importedAt": asset.imported_at,
        "promptCount": count("promptTemplate"),
        "regexCount": count("regex"),
        "toggleCount": toggles,
    })
}

fn response_text(body: Option<&Value>) -> String {
    let Some(Value::Array(blocks)) = body.and_then(|body| body.get("content")) else {
        return String::new();
    };
    blocks
        .iter()
        .filter(|block| block.get("type").and_then(Value::as_str) == Some("text"))
        .filter_map(|block| block.get("text").and_then(Value::as_str))
        .collect()
}

/// 서비스가 입력·출력 정규식에 쓰는 목록. 프리셋은 펼치고 모듈은 flatMap이라 배열이 아니면 값 하나로 넣는다.
fn service_scripts(preset: &Value, modules: &[Asset]) -> Result<Vec<Value>, JsError> {
    let mut scripts = Vec::new();
    match preset.get("regex") {
        None | Some(Value::Null) => {}
        Some(Value::Array(items)) => scripts.extend(items.iter().cloned()),
        Some(Value::String(text)) => scripts.extend(text.chars().map(|c| Value::String(c.to_string()))),
        Some(_) => return Err(JsError("preset.regex is not iterable".into())),
    }
    for module in modules {
        match module.data.get("regex") {
            None | Some(Value::Null) => {}
            Some(Value::Array(items)) => scripts.extend(items.iter().cloned()),
            Some(other) => scripts.push(other.clone()),
        }
    }
    Ok(scripts)
}

fn context_vars(toggles: &Value) -> HashMap<String, String> {
    let Value::Object(fields) = toggles else {
        return HashMap::new();
    };
    fields.iter().filter(|(_, value)| !value.is_null()).map(|(key, value)| (format!("toggle_{key}"), js_string(value))).collect()
}

/// 서비스의 parseContext. 색인 둘 다 지금까지 쌓인 메시지 수다.
fn parse_context<'a>(profile: &'a Profile, character: &'a Character, history: &'a [ChatMessage], vars: &'a HashMap<String, String>) -> CbsContext<'a> {
    CbsContext {
        user_name: Some(&profile.user_name),
        char_name: Some(&character.name),
        vars: Some(vars),
        chat_index: Some(history.len() as f64),
        last_message_index: Some(history.len() as f64),
        messages: Some(history),
        ..Default::default()
    }
}

fn is_safe_integer(value: f64) -> bool {
    is_js_integer(value) && value.abs() <= 9_007_199_254_740_991.0
}

struct CallTarget {
    url: String,
    key: String,
    timeout: Duration,
}

struct DoorInfo {
    url: String,
    token: String,
    model: String,
    context_window: Option<f64>,
    max_output_tokens: f64,
}

impl RoleplayService {
    pub fn new(options: RoleplayOptions) -> Self {
        Self(Arc::new(Inner { options, state: Mutex::default() }))
    }

    fn pool(&self) -> &PgPool {
        &self.0.options.pool
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.0.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn changed(&self, character_id: &str) {
        (self.0.options.broadcast)(json!({ "type": "roleplay.changed", "characterId": character_id }));
    }

    pub fn is_generating(&self, character_id: &str) -> bool {
        self.lock().generating.contains_key(character_id)
    }

    /// 뒤에서 만드는 답이 끝날 때까지 기다린다. 만드는 중이 아니면 바로 끝난다.
    pub async fn wait_for_reply(&self, character_id: &str) {
        let receiver = self.lock().generating.get(character_id).cloned();
        if let Some(mut receiver) = receiver {
            let _ = receiver.wait_for(|done| *done).await;
        }
    }

    pub async fn list_assets(&self) -> Result<Vec<Value>, RoleplayError> {
        let sql = format!("SELECT {ASSET_COLUMNS} FROM roleplay_assets ORDER BY kind, imported_at DESC, id");
        let rows = sqlx::query(sqlx::AssertSqlSafe(sql)).fetch_all(self.pool()).await?;
        rows.iter().map(|row| Ok(asset_summary(&asset_from_row(row)?))).collect()
    }

    pub async fn import_asset(&self, body: &Value) -> Result<Value, RoleplayError> {
        let max = (MAX_IMPORT_BYTES * 4).div_ceil(3) + 4;
        let encoded = text(body.get("dataBase64"), "파일 내용", max, true)?;
        // Node의 base64와 base64url은 같은 해독기라 두 알파벳을 다 받는다.
        let buffer = base64url_decode(encoded);
        if buffer.is_empty() || buffer.len() > MAX_IMPORT_BYTES {
            return Err(bad_request("파일 크기가 올바르지 않습니다."));
        }
        let file_name = match body.get("fileName") {
            None | Some(Value::Null) => String::new(),
            Some(name) => js_string(name),
        };
        let (kind, name, data) = decode_roleplay_asset(&file_name, &buffer)?;
        let sha256 = sha256_hex(&buffer);
        let inserted = format!(
            "INSERT INTO roleplay_assets (kind, name, source_sha256, data)
             VALUES ($1, $2, $3, $4::jsonb)
             ON CONFLICT (source_sha256) DO NOTHING
             RETURNING {ASSET_COLUMNS}"
        );
        let row = sqlx::query(sqlx::AssertSqlSafe(inserted))
            .bind(kind)
            .bind(&name)
            .bind(&sha256)
            .bind(js_json(&data))
            .fetch_optional(self.pool())
            .await?;
        if let Some(row) = row {
            return Ok(json!({ "asset": asset_summary(&asset_from_row(&row)?), "created": true }));
        }
        let existing = format!("SELECT {ASSET_COLUMNS} FROM roleplay_assets WHERE source_sha256 = $1");
        let row = sqlx::query(sqlx::AssertSqlSafe(existing)).bind(&sha256).fetch_optional(self.pool()).await?;
        let Some(row) = row else {
            return Err(RoleplayError::Internal("Cannot read properties of undefined (reading 'data')".into()));
        };
        Ok(json!({ "asset": asset_summary(&asset_from_row(&row)?), "created": false }))
    }

    async fn character(&self, character_id: &str) -> Result<Character, RoleplayError> {
        let row = sqlx::query("SELECT name FROM characters WHERE id = $1").bind(character_id).fetch_optional(self.pool()).await?;
        let Some(row) = row else {
            return Err(RoleplayError::new(404, "직원을 찾을 수 없습니다."));
        };
        Ok(Character { name: row.try_get("name")? })
    }

    async fn profile_row(&self, character_id: &str) -> Result<Profile, RoleplayError> {
        let sql = concat!(
            "SELECT enabled, preset_id::text AS preset_id, module_ids::text[] AS module_ids, toggles, character_description,
                    first_message, user_name, persona, model, local_profile_id, speaker_cue,
                    current_chat_id::text AS current_chat_id, ",
            iso!("updated_at"),
            " AS updated_at FROM roleplay_profiles WHERE character_id = $1"
        );
        let row = sqlx::query(sql).bind(character_id).fetch_optional(self.pool()).await?;
        Ok(match row {
            Some(row) => Profile::from_row(character_id, &row)?,
            None => Profile::defaults(character_id),
        })
    }

    /// 고른 프리셋과 켜진 모듈. 프리셋 행이 없으면 None이다.
    async fn load_assets(&self, profile: &Profile) -> Result<(Option<Value>, Vec<Asset>), RoleplayError> {
        let preset = match profile.preset_id.as_deref().filter(|id| !id.is_empty()) {
            Some(preset_id) => sqlx::query_scalar::<_, Option<Value>>("SELECT data FROM roleplay_assets WHERE id = $1::uuid AND kind = 'preset'")
                .bind(preset_id)
                .fetch_optional(self.pool())
                .await?
                .flatten()
                .filter(|data| !data.is_null()),
            None => None,
        };
        let sql = format!("SELECT {ASSET_COLUMNS} FROM roleplay_assets WHERE kind = 'module' ORDER BY imported_at, id");
        let rows = sqlx::query(sqlx::AssertSqlSafe(sql)).fetch_all(self.pool()).await?;
        let all = rows.iter().map(asset_from_row).collect::<Result<Vec<_>, _>>()?;
        let modules = active_modules(preset.as_ref(), &all, &profile.module_ids).into_iter().cloned().collect();
        Ok((preset, modules))
    }

    async fn messages(&self, chat_id: &str) -> Result<Vec<ChatMessage>, RoleplayError> {
        let sql = concat!(
            "SELECT id::text AS id, role, content, ",
            iso!("created_at"),
            " AS created_at FROM roleplay_messages WHERE chat_id = $1::uuid ORDER BY position"
        );
        let rows = sqlx::query(sql).bind(chat_id).fetch_all(self.pool()).await?;
        rows.iter()
            .map(|row| {
                Ok(ChatMessage {
                    id: row.try_get("id")?,
                    role: row.try_get("role")?,
                    content: row.try_get("content")?,
                    created_at: row.try_get("created_at")?,
                })
            })
            .collect::<Result<Vec<_>, sqlx::Error>>()
            .map_err(RoleplayError::from)
    }

    pub async fn read_profile(&self, character_id: &str) -> Result<Value, RoleplayError> {
        self.character(character_id).await?;
        let profile = self.profile_row(character_id).await?;
        let (preset, modules) = self.load_assets(&profile).await?;
        let mut toggle_definitions: Vec<ToggleDefinition> =
            parse_toggle_definitions(preset.as_ref().and_then(|preset| preset.get("customPromptTemplateToggle")));
        for module in &modules {
            toggle_definitions.extend(parse_toggle_definitions(module.data.get("customModuleToggle")));
        }
        let messages = match profile.current_chat_id.as_deref() {
            Some(chat_id) => self.messages(chat_id).await?,
            None => Vec::new(),
        };
        let (generating, last_error) = {
            let state = self.lock();
            (state.generating.contains_key(character_id), state.last_errors.get(character_id).cloned())
        };
        Ok(json!({
            "profile": profile,
            "toggleDefinitions": toggle_definitions,
            "activeModuleIds": modules.iter().map(|module| module.id.as_str()).collect::<Vec<_>>(),
            "messages": messages,
            "generating": generating,
            "lastError": last_error,
        }))
    }

    pub async fn update_profile(&self, character_id: &str, body: &Value) -> Result<Value, RoleplayError> {
        self.character(character_id).await?;
        let mut next = self.profile_row(character_id).await?;
        if body.is_null() {
            return Err(RoleplayError::Internal("Cannot read properties of null (reading 'enabled')".into()));
        }
        let field = |key: &str| body.get(key);
        if let Some(enabled) = field("enabled") {
            next.enabled = enabled.as_bool().ok_or_else(|| bad_request("enabled는 참/거짓이어야 합니다."))?;
        }
        if let Some(speaker_cue) = field("speakerCue") {
            next.speaker_cue = speaker_cue.as_bool().ok_or_else(|| bad_request("speakerCue는 참/거짓이어야 합니다."))?;
        }
        if let Some(preset_id) = field("presetId") {
            next.preset_id = if preset_id.is_null() { None } else { Some(uuid(preset_id, "presetId")?) };
        }
        if let Some(module_ids) = field("moduleIds") {
            let Value::Array(module_ids) = module_ids else {
                return Err(bad_request("moduleIds는 배열이어야 합니다."));
            };
            let ids = module_ids.iter().map(|id| uuid(id, "moduleIds")).collect::<Result<Vec<_>, _>>()?;
            let mut unique: Vec<String> = Vec::new();
            for id in ids {
                if !unique.contains(&id) {
                    unique.push(id);
                }
            }
            next.module_ids = unique;
        }
        if let Some(toggles) = field("toggles") {
            next.toggles = Value::Object(toggle_values(toggles)?);
        }
        if let Some(value) = field("characterDescription") {
            next.character_description = text(Some(value), "캐릭터 설명", MAX_TEXT, false)?.to_string();
        }
        if let Some(value) = field("firstMessage") {
            next.first_message = text(Some(value), "첫 메시지", MAX_TEXT, false)?.to_string();
        }
        if let Some(value) = field("persona") {
            next.persona = text(Some(value), "페르소나", MAX_TEXT, false)?.to_string();
        }
        if let Some(value) = field("userName") {
            next.user_name = js_trim(text(Some(value), "사용자 이름", 100, true)?).to_string();
        }
        if let Some(value) = field("model") {
            next.model = js_trim(text(Some(value), "모델", 200, true)?).to_string();
        }
        if let Some(value) = field("localProfileId") {
            // 빈 문자열과 null은 안티그래비티 프록시로 되돌린다.
            next.local_profile_id = match value {
                Value::Null => None,
                Value::String(text) if text.is_empty() => None,
                value => Some(text(Some(value), "로컬 프로필", 200, false)?.to_string()),
            };
            if let Some(local_profile_id) = next.local_profile_id.clone() {
                self.local_definition(&local_profile_id, character_id).await?;
            }
        }

        let asset_ids: Vec<String> = next.preset_id.iter().chain(&next.module_ids).filter(|id| !id.is_empty()).cloned().collect();
        if !asset_ids.is_empty() {
            let rows = sqlx::query("SELECT id::text AS id, kind FROM roleplay_assets WHERE id = ANY($1::text[]::uuid[])")
                .bind(&asset_ids)
                .fetch_all(self.pool())
                .await?;
            let mut kinds = HashMap::new();
            for row in &rows {
                kinds.insert(row.try_get::<String, _>("id")?, row.try_get::<String, _>("kind")?);
            }
            let kind_of = |id: &str| kinds.get(id).map(String::as_str);
            if let Some(preset_id) = next.preset_id.as_deref().filter(|id| !id.is_empty())
                && kind_of(preset_id) != Some("preset")
            {
                return Err(bad_request("가져온 프리셋이 아닙니다."));
            }
            if next.module_ids.iter().any(|id| kind_of(id) != Some("module")) {
                return Err(bad_request("가져온 모듈이 아닙니다."));
            }
        }

        sqlx::query(
            "INSERT INTO roleplay_profiles (
               character_id, enabled, preset_id, module_ids, toggles, character_description,
               first_message, user_name, persona, model, current_chat_id, local_profile_id, speaker_cue, updated_at
             ) VALUES ($1, $2, $3::uuid, $4::text[]::uuid[], $5::jsonb, $6, $7, $8, $9, $10, $11::uuid, $12, $13, now())
             ON CONFLICT (character_id) DO UPDATE SET
               enabled = EXCLUDED.enabled,
               preset_id = EXCLUDED.preset_id,
               module_ids = EXCLUDED.module_ids,
               toggles = EXCLUDED.toggles,
               character_description = EXCLUDED.character_description,
               first_message = EXCLUDED.first_message,
               user_name = EXCLUDED.user_name,
               persona = EXCLUDED.persona,
               model = EXCLUDED.model,
               local_profile_id = EXCLUDED.local_profile_id,
               speaker_cue = EXCLUDED.speaker_cue,
               updated_at = now()",
        )
        .bind(character_id)
        .bind(next.enabled)
        .bind(&next.preset_id)
        .bind(&next.module_ids)
        .bind(js_json(&next.toggles))
        .bind(&next.character_description)
        .bind(&next.first_message)
        .bind(&next.user_name)
        .bind(&next.persona)
        .bind(&next.model)
        .bind(&next.current_chat_id)
        .bind(&next.local_profile_id)
        .bind(next.speaker_cue)
        .execute(self.pool())
        .await?;
        self.changed(character_id);
        self.read_profile(character_id).await
    }

    async fn create_chat(&self, character_id: &str) -> Result<String, RoleplayError> {
        sqlx::query("INSERT INTO roleplay_profiles (character_id) VALUES ($1) ON CONFLICT (character_id) DO NOTHING")
            .bind(character_id)
            .execute(self.pool())
            .await?;
        let chat_id: String = sqlx::query_scalar("INSERT INTO roleplay_chats (character_id) VALUES ($1) RETURNING id::text")
            .bind(character_id)
            .fetch_one(self.pool())
            .await?;
        sqlx::query("UPDATE roleplay_profiles SET current_chat_id = $2::uuid, updated_at = now() WHERE character_id = $1")
            .bind(character_id)
            .bind(&chat_id)
            .execute(self.pool())
            .await?;
        Ok(chat_id)
    }

    pub async fn start_new_chat(&self, character_id: &str) -> Result<Value, RoleplayError> {
        self.character(character_id).await?;
        if self.is_generating(character_id) {
            return Err(RoleplayError::new(409, "답을 만드는 중에는 새 대화를 시작할 수 없습니다."));
        }
        let chat_id = self.create_chat(character_id).await?;
        // 지난 대화는 남기지 않는다. 메시지는 외래 키로 함께 지워진다(2026-10-28 요청).
        sqlx::query("DELETE FROM roleplay_chats WHERE character_id = $1 AND id <> $2::uuid")
            .bind(character_id)
            .bind(&chat_id)
            .execute(self.pool())
            .await?;
        self.lock().last_errors.remove(character_id);
        self.changed(character_id);
        self.read_profile(character_id).await
    }

    /// 사용자 메시지를 저장하고 바로 돌려준 뒤, 답은 뒤에서 만들어 저장한다.
    pub async fn send_message(&self, character_id: &str, body: &Value) -> Result<Value, RoleplayError> {
        let character_id = character_id.to_string();
        if body.is_null() {
            return Err(RoleplayError::Internal("Cannot read properties of null (reading 'content')".into()));
        }
        let content = text(body.get("content"), "메시지", MAX_MESSAGE, true)?.to_string();
        let character = self.character(&character_id).await?;
        if self.is_generating(&character_id) {
            return Err(RoleplayError::new(409, "이전 답을 만드는 중입니다."));
        }
        let mut profile = self.profile_row(&character_id).await?;
        if profile.preset_id.as_deref().is_none_or(str::is_empty) {
            return Err(RoleplayError::new(409, "롤플레이 프리셋을 먼저 고르세요."));
        }
        let (preset, modules) = self.load_assets(&profile).await?;
        let Some(preset) = preset else {
            return Err(RoleplayError::new(409, "고른 프리셋을 찾을 수 없습니다."));
        };

        // Node는 확인과 등록 사이에 await가 있어 동시 요청이 함께 지나갈 수 있다. 여기서는 등록할 때 한 번 더 막는다.
        let (done, receiver) = watch::channel(false);
        {
            let mut state = self.lock();
            if state.generating.contains_key(&character_id) {
                return Err(RoleplayError::new(409, "이전 답을 만드는 중입니다."));
            }
            state.generating.insert(character_id.clone(), receiver);
        }
        let stored: Result<String, RoleplayError> = async {
            if profile.current_chat_id.is_none() {
                profile.current_chat_id = Some(self.create_chat(&character_id).await?);
            }
            let chat_id = profile.current_chat_id.clone().unwrap_or_default();
            let history = self.messages(&chat_id).await?;
            let input = {
                let vars = context_vars(&profile.toggles);
                let context = parse_context(&profile, &character, &history, &vars);
                apply_regex_scripts(&service_scripts(&preset, &modules)?, &content, "editinput", &context)?
            };
            sqlx::query("INSERT INTO roleplay_messages (chat_id, role, content) VALUES ($1::uuid, 'user', $2)")
                .bind(&chat_id)
                .bind(&input)
                .execute(self.pool())
                .await?;
            Ok(chat_id)
        }
        .await;
        let chat_id = match stored {
            Ok(chat_id) => chat_id,
            Err(error) => {
                self.lock().generating.remove(&character_id);
                return Err(error);
            }
        };
        self.lock().last_errors.remove(&character_id);
        self.changed(&character_id);
        let service = self.clone();
        tokio::spawn(async move {
            if let Err(error) = service.generate(&character_id, &character, &profile, &preset, &modules).await {
                let message = match error {
                    RoleplayError::Status(_, message) => message,
                    RoleplayError::Internal(message) => {
                        eprintln!("롤플레이 답 생성 실패: {message}");
                        NOT_GENERATED.to_string()
                    }
                };
                service.lock().last_errors.insert(character_id.clone(), message);
            }
            service.lock().generating.remove(&character_id);
            service.changed(&character_id);
            let _ = done.send(true);
        });
        Ok(json!({ "accepted": true, "chatId": chat_id }))
    }

    async fn generate(&self, character_id: &str, character: &Character, profile: &Profile, preset: &Value, modules: &[Asset]) -> Result<(), RoleplayError> {
        let chat_id = profile.current_chat_id.clone().unwrap_or_default();
        let history = self.messages(&chat_id).await?;
        let module_refs: Vec<&Asset> = modules.iter().collect();
        let build = |drop_oldest: usize| -> Result<Value, RoleplayError> {
            let formatted = assemble_roleplay_messages(&AssembleInput {
                preset,
                modules: &module_refs,
                toggles: &profile.toggles,
                character_name: &character.name,
                character_description: &profile.character_description,
                first_message: &profile.first_message,
                user_name: &profile.user_name,
                persona: &profile.persona,
                speaker_cue: profile.speaker_cue,
                messages: &history,
                drop_oldest,
            })?;
            Ok(roleplay_request_body(preset, &profile.model, &formatted)?)
        };
        // 첫 메시지까지 셀 수 있게 하나 더 잡는다. 실제로 빼는 수는 조립할 때 다시 줄인다.
        let reply = match profile.local_profile_id.as_deref().filter(|id| !id.is_empty()) {
            Some(local_profile_id) => {
                let definition = self.local_definition(local_profile_id, character_id).await?;
                self.call_local_model(&build, history.len() + 1, preset, &definition).await?
            }
            None => self.call_model(&build(0)?, None).await?,
        };
        let output = {
            let vars = context_vars(&profile.toggles);
            let context = parse_context(profile, character, &history, &vars);
            apply_regex_scripts(&service_scripts(preset, modules)?, &reply, "editoutput", &context)?
        };
        sqlx::query("INSERT INTO roleplay_messages (chat_id, role, content) VALUES ($1::uuid, 'char', $2)")
            .bind(&chat_id)
            .bind(&output)
            .execute(self.pool())
            .await?;
        Ok(())
    }

    /// 직원 업무와 같은 프로필을 쓰면 그 직원의 주소·추론 설정을 따라야 같은 로컬 연결을 함께 쓴다.
    async fn local_definition(&self, local_profile_id: &str, character_id: &str) -> Result<LocalDefinition, RoleplayError> {
        let row = sqlx::query(
            "SELECT p.definition, c.config
               FROM local_agent_profiles p
               LEFT JOIN characters c ON c.id = $2
              WHERE p.id = $1 AND p.enabled = true",
        )
        .bind(local_profile_id)
        .bind(character_id)
        .fetch_optional(self.pool())
        .await?;
        let Some(row) = row else {
            return Err(bad_request("로컬 프로필이 꺼져 있거나 없습니다."));
        };
        let raw = row.try_get::<Option<Value>, _>("definition")?.unwrap_or(Value::Null);
        let config = row.try_get::<Option<Value>, _>("config")?.filter(|config| !config.is_null()).unwrap_or_else(|| json!({}));
        let unreadable = |_| bad_request("로컬 프로필 설정을 읽지 못했습니다.");
        let mut value = normalize_local_definition(&raw).map_err(unreadable)?.to_value();
        // 주소는 프로필이 아니라 직원의 PC 경로라 프로필이 달라도 따른다. 추론 설정은 같은 프로필일 때만 따른다.
        let reasoning = (config.get("localProfileId").and_then(Value::as_str) == Some(local_profile_id))
            .then(|| config.get("localReasoning"))
            .flatten()
            .filter(|reasoning| js_truthy(Some(reasoning)));
        if let Some(reasoning) = reasoning {
            value["profile"]["reasoning"] = reasoning.clone();
        }
        if let Some(address) = config.get("localHostAddress").filter(|address| js_truthy(Some(address))) {
            value["host"]["address"] = address.clone();
        }
        let definition = normalize_local_definition(&value).map_err(unreadable)?;
        if definition.profile.backend != "claude" {
            return Err(bad_request("Messages 형식을 쓰는 로컬 프로필만 롤플레이에 연결할 수 있습니다."));
        }
        Ok(definition)
    }

    async fn call_local_model(
        &self,
        build: &(impl Fn(usize) -> Result<Value, RoleplayError> + Sync),
        max_drop: usize,
        preset: &Value,
        definition: &LocalDefinition,
    ) -> Result<String, RoleplayError> {
        let Some(providers) = (self.0.options.local_doors)() else {
            return Err(RoleplayError::new(503, "로컬 모델 서비스가 준비되지 않았습니다."));
        };
        let door = providers.open_door(&definition.to_value()).await.map_err(|message| {
            RoleplayError::new(503, if message.is_empty() { "로컬 모델 연결을 열지 못했습니다.".to_string() } else { message })
        })?;
        let release = door.release;
        let door = DoorInfo {
            url: door.url,
            token: door.token,
            model: door.model,
            context_window: door.context_window,
            max_output_tokens: door.max_output_tokens,
        };
        let result = async {
            // 로컬 브리지는 프로필 모델명과 출력 한도를 그대로 요구한다.
            let body_for = |drop_oldest: usize| -> Result<Value, RoleplayError> {
                let mut body = build(drop_oldest)?;
                let requested = match body.get("max_tokens") {
                    None | Some(Value::Null) => door.max_output_tokens,
                    Some(value) => js_to_number(value),
                };
                if let Value::Object(fields) = &mut body {
                    fields.insert("model".into(), json!(door.model));
                    fields.insert("max_tokens".into(), js_number(js_min(requested, door.max_output_tokens)));
                }
                Ok(body)
            };
            let body = self.fit_local_context(&body_for, max_drop, preset, &door).await?;
            let target = CallTarget { url: door.url.clone(), key: door.token.clone(), timeout: LOCAL_REQUEST_TIMEOUT };
            self.call_model(&body, Some(target)).await
        }
        .await;
        release().await.map_err(RoleplayError::Internal)?;
        result
    }

    /// 프로필 컨텍스트와 프리셋 최대 컨텍스트 중 작은 쪽에서 답 한도를 뺀 만큼만 입력으로 보낸다.
    async fn fit_local_context(
        &self,
        body_for: &(impl Fn(usize) -> Result<Value, RoleplayError> + Sync),
        max_drop: usize,
        preset: &Value,
        door: &DoorInfo,
    ) -> Result<Value, RoleplayError> {
        let preset_window = preset.get("maxContext").map_or(f64::NAN, js_to_number);
        let preset_window = if preset_window > 0.0 { preset_window } else { f64::INFINITY };
        let door_window = door.context_window.filter(|window| *window > 0.0).unwrap_or(f64::INFINITY);
        let window = js_min(door_window, preset_window);
        if !window.is_finite() {
            return body_for(0);
        }
        let max_tokens = body_for(0)?.get("max_tokens").map_or(f64::NAN, js_to_number);
        let limit = window - max_tokens - CONTEXT_MARGIN_TOKENS;
        let fit = fit_roleplay_context(body_for, max_drop, limit, |body: Value| async move { self.count_local_tokens(&body, door).await }).await?;
        fit.body.ok_or_else(|| {
            RoleplayError::new(
                413,
                format!(
                    "프리셋과 마지막 메시지만으로도 로컬 모델 한도를 넘습니다({}토큰, 한도 {}토큰).",
                    number_to_string(fit.tokens.unwrap_or(f64::NAN)),
                    number_to_string(limit)
                ),
            )
        })
    }

    async fn count_local_tokens(&self, body: &Value, door: &DoorInfo) -> Result<f64, RoleplayError> {
        let request = FetchRequest {
            method: "POST",
            url: format!("{}/count_tokens", door.url),
            headers: vec![
                ("content-type", "application/json".into()),
                ("anthropic-version", "2023-06-01".into()),
                ("x-api-key", door.token.clone()),
            ],
            body: Some(js_json(body)),
            timeout: LOCAL_REQUEST_TIMEOUT,
        };
        let failed = "로컬 모델에서 입력 토큰 수를 세지 못했습니다.";
        let response = self.0.options.fetch.fetch(request).await.map_err(|_| RoleplayError::new(502, failed))?;
        let raw = response.body.clone().map_err(RoleplayError::Internal)?;
        let tokens = parse_js_json(&raw).ok().and_then(|parsed| parsed.get("input_tokens").map(js_to_number)).unwrap_or(f64::NAN);
        if !response.ok() || !is_safe_integer(tokens) || tokens < 0.0 {
            return Err(RoleplayError::new(502, format!("로컬 모델에서 입력 토큰 수를 세지 못했습니다({}).", response.status)));
        }
        Ok(tokens)
    }

    /// 프록시에 연결부터 못 했으면 한 번 켜 보고 같은 요청을 다시 보낸다. 요청이 나가지 않았을 때만이라 겹쳐 보내지 않는다.
    async fn fetch_proxy(&self, request: FetchRequest) -> Result<FetchResponse, FetchFailure> {
        let options = &self.0.options;
        match options.fetch.fetch(request.clone()).await {
            Err(FetchFailure::Unreachable) => match &options.proxy_starter {
                Some(starter) if starter.start().await => options.fetch.fetch(request).await,
                _ => Err(FetchFailure::Unreachable),
            },
            result => result,
        }
    }

    async fn read_key(&self) -> Result<String, RoleplayError> {
        let bytes = tokio::fs::read(&self.0.options.api_key_file).await.map_err(|_| RoleplayError::new(503, KEY_FILE_UNREADABLE))?;
        Ok(js_trim(&String::from_utf8_lossy(&bytes)).to_string())
    }

    /// 프록시가 계정에서 받아 온 모델을 그대로 보여 준다. 키는 응답에 넣지 않는다.
    pub async fn list_proxy_models(&self) -> Result<Value, RoleplayError> {
        let key = self.read_key().await?;
        let url = reqwest::Url::parse(&self.0.options.api_url).and_then(|base| base.join("models"));
        let response = match url {
            Ok(url) => {
                let request = FetchRequest {
                    method: "GET",
                    url: url.to_string(),
                    headers: vec![("anthropic-version", "2023-06-01".into()), ("x-api-key", key)],
                    body: None,
                    timeout: MODEL_LIST_TIMEOUT,
                };
                self.fetch_proxy(request).await.ok()
            }
            Err(_) => None,
        };
        let Some(response) = response else {
            return Err(RoleplayError::new(502, PROXY_UNREACHABLE));
        };
        let body = response.body.as_ref().ok().and_then(|raw| parse_js_json(raw).ok());
        let data = body.as_ref().and_then(|body| body.get("data")).and_then(Value::as_array);
        let (true, Some(data)) = (response.ok(), data) else {
            return Err(RoleplayError::new(502, format!("모델 목록을 받지 못했습니다({}).", response.status)));
        };
        let mut models: Vec<(String, String)> = data
            .iter()
            .filter_map(|entry| {
                let id = entry.get("id")?.as_str().filter(|id| !js_trim(id).is_empty())?;
                let title = match entry.get("description") {
                    Some(Value::String(description)) if !js_trim(description).is_empty() => description.clone(),
                    _ => id.to_string(),
                };
                Some((id.to_string(), title))
            })
            .collect();
        js_sort_by(&mut models, |left, right| match locale_compare(&left.0, &right.0) {
            Ordering::Less => -1.0,
            Ordering::Equal => 0.0,
            Ordering::Greater => 1.0,
        });
        let models: Vec<Value> = models.into_iter().map(|(id, title)| json!({ "id": id, "title": title })).collect();
        Ok(json!({ "models": models }))
    }

    async fn call_model(&self, request_body: &Value, target: Option<CallTarget>) -> Result<String, RoleplayError> {
        let api_url = &self.0.options.api_url;
        let CallTarget { url, key, timeout } = match target {
            Some(target) => target,
            None => CallTarget { url: api_url.clone(), key: self.read_key().await?, timeout: REQUEST_TIMEOUT },
        };
        let request = FetchRequest {
            method: "POST",
            url: url.clone(),
            headers: vec![
                ("content-type", "application/json".into()),
                ("anthropic-version", "2023-06-01".into()),
                ("x-api-key", key),
            ],
            body: Some(js_json(request_body)),
            timeout,
        };
        let fetched = if url == *api_url { self.fetch_proxy(request).await } else { self.0.options.fetch.fetch(request).await };
        let response = fetched.map_err(|failure| {
            RoleplayError::new(
                502,
                match failure {
                    FetchFailure::Timeout => "모델 응답이 너무 오래 걸려 멈췄습니다.",
                    FetchFailure::Unreachable | FetchFailure::Failed if url == *api_url => PROXY_UNREACHABLE,
                    FetchFailure::Unreachable | FetchFailure::Failed => "로컬 모델에 연결하지 못했습니다.",
                },
            )
        })?;
        let raw = response.body.clone().map_err(RoleplayError::Internal)?;
        let body = parse_js_json(&raw).ok();
        if !response.ok() {
            // 로컬 문은 error를 문자열로, 프록시는 error.message로 보낸다.
            let message = body.as_ref().and_then(|body| body.get("error")).and_then(|error| match error {
                Value::String(message) => Some(message.as_str()),
                error => error.get("message").and_then(Value::as_str),
            });
            let detail = message.map(|message| utf16_prefix(message, 300)).unwrap_or_default();
            let detail = if detail.is_empty() { String::new() } else { format!(" {detail}") };
            return Err(RoleplayError::new(502, format!("모델 서버가 {}로 거절했습니다.{detail}", response.status)));
        }
        let reply = response_text(body.as_ref());
        if js_trim(&reply).is_empty() {
            return Err(RoleplayError::new(502, "모델이 빈 답을 보냈습니다."));
        }
        Ok(reply)
    }
}

#[cfg(test)]
mod tests;
