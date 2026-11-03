// Telegram 개인 대화를 4317 직원 업무·롤플레이 API에 잇는다. Node telegram-bridge.mjs를 옮기며 토큰은 오류·로그에 넣지 않는다.

pub mod cli;

#[cfg(test)]
mod tests;

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use futures_util::FutureExt;
use futures_util::future::{BoxFuture, try_join_all};
use serde_json::{Map, Value, json};

use crate::js::{
    encode_uri_component, is_js_whitespace, js_keys, js_string, js_to_number, js_trim, js_truthy, node_fs_message, now_milliseconds,
    number_to_string, parse_js_json, to_js_json,
};

const TERMINAL_STATUSES: [&str; 3] = ["completed", "failed", "interrupted"];
pub const TELEGRAM_API_URL: &str = "https://api.telegram.org";
pub const BACKEND_URL: &str = "http://127.0.0.1:4317";
pub const TEXT_LIMIT: usize = 3_800;
const MAX_ROUTES: usize = 500;
const MENU_TEXT: &str = "대화할 직원을 선택하세요. 선택 후 메시지를 보내면 직원의 답변이 여기로 옵니다.\n\n/office 직원 선택 · /status 현재 상태 · /new 선택한 직원과 새 대화\n직원 이름: 내용 형식이나 직원 답장에 답글을 달아도 됩니다.";

/// Node Error에 붙이던 status·retryAfter·uncertain을 함께 담는 오류. 문구에는 토큰·요청 주소를 넣지 않는다.
#[derive(Clone, Debug, Default)]
pub struct CallError {
    pub message: String,
    pub status: Option<u16>,
    pub retry_after: Option<Value>,
    pub uncertain: bool,
}

impl CallError {
    pub fn new(message: impl Into<String>) -> Self {
        Self { message: message.into(), ..Self::default() }
    }

    /// 로그에 남기는 `error.status ?? 'connection'`.
    pub fn status_label(&self) -> String {
        self.status.map_or_else(|| "connection".into(), |status| status.to_string())
    }
}

/// `telegram(method, body)`. 결과(payload.result)가 없으면 null이다.
pub type TelegramCall = Arc<dyn Fn(&str, Value) -> BoxFuture<'static, Result<Value, CallError>> + Send + Sync>;
/// `backend(path, body)`. body가 있으면 POST, 없으면 GET이다.
pub type BackendCall = Arc<dyn Fn(&str, Option<Value>) -> BoxFuture<'static, Result<Value, CallError>> + Send + Sync>;
/// `persist(state)`. 실패 문구는 Node fs 오류처럼 위로 올라간다.
pub type Persist = Arc<dyn Fn(&Value) -> Result<(), String> + Send + Sync>;
pub type Logger = Arc<dyn Fn(LogLevel, &str) + Send + Sync>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LogLevel {
    /// console.log(표준 출력)
    Log,
    /// console.warn(표준 오류)
    Warn,
    /// console.error(표준 오류)
    Error,
}

/// Node console과 같은 출력 위치를 쓰는 기록기.
pub fn console_logger() -> Logger {
    Arc::new(|level, line| match level {
        LogLevel::Log => println!("{line}"),
        LogLevel::Warn | LogLevel::Error => eprintln!("{line}"),
    })
}

/// `readTelegramConfiguration(file)`. 셸 코드를 실행하지 않고 `KEY=값` 줄만 읽으며 TELEGRAM_BOT_TOKEN만 돌려준다.
pub fn read_telegram_configuration(file: &str) -> Result<String, String> {
    let bytes = std::fs::read(file).map_err(|error| match error.kind() {
        std::io::ErrorKind::IsADirectory => "EISDIR: illegal operation on a directory, read".to_string(),
        _ => node_fs_message(&error, "open", file),
    })?;
    let mut token = None;
    for line in String::from_utf8_lossy(&bytes).split('\n') {
        let line = line.strip_suffix('\r').unwrap_or(line);
        if let Some((key, value)) = environment_line(line) {
            if key == "TELEGRAM_BOT_TOKEN" {
                token = Some(value);
            }
        }
    }
    token.filter(|token| !token.is_empty()).ok_or_else(|| "Telegram token is missing".to_string())
}

/// `/^\s*(?:export\s+)?([A-Z_][A-Z_0-9]*)\s*=\s*(.*?)\s*$/`과 `/^(['"])(.*)\1$/` 따옴표 벗기기.
/// `.`은 줄 끝 문자(\n·\r·U+2028·U+2029)를 건너지 못하므로 값 가운데에 그런 문자가 있으면 맞지 않는다.
fn environment_line(line: &str) -> Option<(&str, String)> {
    let rest = line.trim_start_matches(is_js_whitespace);
    let rest = match rest.strip_prefix("export") {
        Some(after) if after.starts_with(is_js_whitespace) => {
            let after = after.trim_start_matches(is_js_whitespace);
            if after.starts_with(|c: char| c.is_ascii_uppercase() || c == '_') { after } else { rest }
        }
        _ => rest,
    };
    let name_length = rest
        .char_indices()
        .take_while(|(index, c)| c.is_ascii_uppercase() || *c == '_' || (*index > 0 && c.is_ascii_digit()))
        .count();
    if name_length == 0 {
        return None;
    }
    let (name, after) = rest.split_at(name_length);
    let after = after.trim_start_matches(is_js_whitespace).strip_prefix('=')?;
    let value = js_trim(after);
    if value.contains(['\n', '\r', '\u{2028}', '\u{2029}']) {
        return None;
    }
    let unquoted = match value.chars().next() {
        Some(quote @ ('\'' | '"')) if value.len() >= 2 && value.ends_with(quote) => &value[1..value.len() - 1],
        _ => value,
    };
    Some((name, unquoted.to_string()))
}

/// Node fetch(undici)의 `response.json()`. UTF-8로 풀고 앞의 BOM을 지운 뒤 JSON.parse 한다. 읽기·해석 실패는 {}다.
async fn response_json(response: reqwest::Response) -> Value {
    match response.bytes().await {
        Ok(bytes) => {
            let text = String::from_utf8_lossy(&bytes);
            parse_js_json(text.strip_prefix('\u{feff}').unwrap_or(&text)).unwrap_or_else(|_| json!({}))
        }
        Err(_) => json!({}),
    }
}

fn http_client() -> Result<reqwest::Client, String> {
    // Node fetch는 프록시 환경 변수를 쓰지 않고 3xx를 20번까지 따라간다.
    crate::http_client_builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::limited(20))
        .build()
        .map_err(|error| error.to_string())
}

/// `telegramClient(token)`. 연결 실패는 결과가 불명확(uncertain)하고, 응답 오류는 상태 코드와 retry_after를 남긴다.
/// reqwest 오류 문구에는 요청 주소(봇 토큰)가 들어가므로 고정 문구만 쓴다.
pub fn telegram_client(token: &str, api_url: &str) -> Result<TelegramCall, String> {
    let http = http_client()?;
    let prefix = format!("{api_url}/bot{token}/");
    Ok(Arc::new(move |method: &str, body: Value| {
        let http = http.clone();
        let url = format!("{prefix}{method}");
        let method = method.to_string();
        async move {
            let seconds = if method == "getUpdates" { 35 } else { 20 };
            let connection_failed = || CallError { uncertain: true, ..CallError::new(format!("Telegram {method} connection failed")) };
            let bytes = to_js_json(&body).map_err(|_| connection_failed())?;
            let response = http
                .post(url)
                .header("content-type", "application/json")
                .body(bytes)
                .timeout(Duration::from_secs(seconds))
                .send()
                .await
                .map_err(|_| connection_failed())?;
            let status = response.status().as_u16();
            let ok = response.status().is_success();
            let payload = response_json(response).await;
            if payload.is_null() {
                let key = if ok { "ok" } else { "parameters" };
                return Err(CallError::new(format!("Cannot read properties of null (reading '{key}')")));
            }
            if !ok || !js_truthy(payload.get("ok")) {
                let retry_after = payload.get("parameters").and_then(|parameters| parameters.get("retry_after")).cloned();
                return Err(CallError { status: Some(status), retry_after, ..CallError::new(format!("Telegram {method} failed ({status})")) });
            }
            Ok(payload.get("result").cloned().unwrap_or(Value::Null))
        }
        .boxed()
    }))
}

/// `backendClient(base)`. http 루프백 주소만 받는다. POST 연결 실패만 결과가 불명확하다.
pub fn backend_client(base: &str) -> Result<BackendCall, String> {
    let url = url::Url::parse(base).map_err(|_| "Invalid URL".to_string())?;
    if url.scheme() != "http" || !matches!(url.host_str(), Some("127.0.0.1" | "localhost" | "[::1]")) {
        return Err("Telegram bridge requires a loopback backend".into());
    }
    let http = http_client()?;
    Ok(Arc::new(move |path: &str, body: Option<Value>| {
        let http = http.clone();
        let target = url.join(path);
        async move {
            let connection_failed = || CallError { uncertain: body.is_some(), ..CallError::new("OFFICESTRA backend connection failed") };
            let target = target.map_err(|_| connection_failed())?;
            let request = match &body {
                Some(body) => http
                    .post(target)
                    .header("content-type", "application/json")
                    .body(to_js_json(body).map_err(|_| connection_failed())?),
                None => http.get(target),
            };
            let response = request.timeout(Duration::from_secs(120)).send().await.map_err(|_| connection_failed())?;
            let status = response.status();
            let data = response_json(response).await;
            if !status.is_success() {
                let status = status.as_u16();
                return Err(CallError { status: Some(status), ..CallError::new(format!("OFFICESTRA request failed ({status})")) });
            }
            Ok(data)
        }
        .boxed()
    }))
}

/// `splitTelegramText(text, limit)`. 코드 포인트를 쪼개지 않고 조각마다 UTF-16 단위 수가 limit 이하가 되게 나눈다.
pub fn split_telegram_text(text: &str, limit: usize) -> Vec<String> {
    let mut parts = Vec::new();
    let mut part = String::new();
    let mut units = 0;
    for point in text.chars() {
        let length = point.len_utf16();
        if units + length > limit {
            parts.push(std::mem::take(&mut part));
            units = 0;
        }
        part.push(point);
        units += length;
    }
    if !part.is_empty() {
        parts.push(part);
    }
    parts
}

fn type_error(message: impl Into<String>) -> CallError {
    CallError::new(message)
}

/// `value?.key`. 없는 값·null·객체가 아닌 값에서는 undefined다.
fn prop<'a>(value: Option<&'a Value>, key: &str) -> Option<&'a Value> {
    match value {
        Some(Value::Object(fields)) => fields.get(key),
        _ => None,
    }
}

/// `value.key`. undefined·null이면 TypeError다.
fn strict_prop<'a>(value: Option<&'a Value>, key: &str) -> Result<Option<&'a Value>, CallError> {
    match value {
        None => Err(type_error(format!("Cannot read properties of undefined (reading '{key}')"))),
        Some(Value::Null) => Err(type_error(format!("Cannot read properties of null (reading '{key}')"))),
        other => Ok(prop(other, key)),
    }
}

fn nullish(value: Option<&Value>) -> bool {
    matches!(value, None | Some(Value::Null))
}

/// `a ?? b`.
fn coalesce<'a>(value: Option<&'a Value>, fallback: Option<&'a Value>) -> Option<&'a Value> {
    if nullish(value) { fallback } else { value }
}

/// 템플릿 문자열·String()의 글자. undefined는 "undefined"다.
fn template(value: Option<&Value>) -> String {
    value.map_or_else(|| "undefined".into(), js_string)
}

/// `a === b`. 객체·배열은 서로 다른 참조로 본다.
fn strict_equal(left: Option<&Value>, right: Option<&Value>) -> bool {
    match (left, right) {
        (None, None) | (Some(Value::Null), Some(Value::Null)) => true,
        (Some(Value::Bool(left)), Some(Value::Bool(right))) => left == right,
        (Some(Value::Number(left)), Some(Value::Number(right))) => left.as_f64() == right.as_f64(),
        (Some(Value::String(left)), Some(Value::String(right))) => left == right,
        _ => false,
    }
}

fn is_text(value: Option<&Value>, text: &str) -> bool {
    matches!(value, Some(Value::String(found)) if found == text)
}

/// 없는 값은 NaN인 `Number(value)`.
fn number_of(value: Option<&Value>) -> f64 {
    value.map_or(f64::NAN, js_to_number)
}

/// JS 숫자를 JSON 값으로. 정수는 정수로, NaN·Infinity는 JSON.stringify처럼 null로 둔다.
fn number_value(number: f64) -> Value {
    if number.is_finite() && number.fract() == 0.0 && number.abs() <= 9_007_199_254_740_992.0 {
        json!(number as i64)
    } else {
        serde_json::Number::from_f64(number).map_or(Value::Null, Value::Number)
    }
}

/// `Number.isSafeInteger(value)`.
fn safe_integer(value: Option<&Value>) -> Option<f64> {
    let number = match value {
        Some(Value::Number(number)) => number.as_f64()?,
        _ => return None,
    };
    (number.fract() == 0.0 && number.abs() <= 9_007_199_254_740_991.0).then_some(number)
}

/// `value?.trim()`. 문자열이 아닌 값이면 TypeError다.
fn optional_trim(value: Option<&Value>, label: &str) -> Result<Option<String>, CallError> {
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) => Ok(Some(js_trim(text).to_string())),
        Some(_) => Err(type_error(format!("{label}?.trim is not a function"))),
    }
}

/// 배열이어야 하는 값(`list ?? []`의 결과). null·undefined는 빈 배열, 그 밖의 값은 TypeError다.
fn array_items(value: Option<&Value>, label: &str) -> Result<Vec<Value>, CallError> {
    match value {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::Array(items)) => Ok(items.clone()),
        Some(_) => Err(type_error(format!("{label} is not a function"))),
    }
}

/// JSON.stringify가 쓰는 JS 객체 키 순서(정수 모양 키 오름차순 먼저)로 다시 놓은 사본.
fn js_ordered(value: &Value) -> Value {
    match value {
        Value::Object(fields) => Value::Object(js_keys(fields).into_iter().map(|key| (key.to_string(), js_ordered(&fields[key]))).collect()),
        Value::Array(items) => Value::Array(items.iter().map(js_ordered).collect()),
        other => other.clone(),
    }
}

fn insert_defined(fields: &mut Map<String, Value>, key: &str, value: Option<&Value>) {
    if let Some(value) = value {
        fields.insert(key.into(), value.clone());
    }
}

fn object_mut<'a>(value: Option<&'a mut Value>, label: &str) -> Result<&'a mut Map<String, Value>, CallError> {
    match value {
        Some(Value::Object(fields)) => Ok(fields),
        _ => Err(type_error(format!("Cannot set properties of {label}"))),
    }
}

/// `state.chats[chatID] ??= { selected: 'boss', conversations: {}, routes: {} }`.
fn chat_state<'a>(state: &'a mut Value, chat: &str) -> Result<&'a mut Map<String, Value>, CallError> {
    let chats = object_mut(state.get_mut("chats"), "state.chats")?;
    if nullish(chats.get(chat)) {
        chats.insert(chat.into(), json!({ "selected": "boss", "conversations": {}, "routes": {} }));
    }
    object_mut(chats.get_mut(chat), "the chat state")
}

fn pending_map(state: &mut Value) -> Result<&mut Map<String, Value>, CallError> {
    object_mut(state.get_mut("pending"), "state.pending")
}

/// 업무 중 Node가 같은 객체를 고치던 `pending`을 상태에 되돌려 놓는다. 이미 지워진 항목이면 그대로 둔다.
fn write_back(state: &mut Value, key: &str, pending: &Map<String, Value>) {
    if let Some(Value::Object(fields)) = state.get_mut("pending") {
        if let Some(slot) = fields.get_mut(key) {
            *slot = Value::Object(pending.clone());
        }
    }
}

/// `active.messages?.at(-1)?.id ?? null`.
fn last_message_id(messages: Option<&Value>) -> Result<Value, CallError> {
    match messages {
        None | Some(Value::Null) | Some(Value::String(_)) => Ok(Value::Null),
        Some(Value::Array(items)) => Ok(coalesce(prop(items.last(), "id"), None).cloned().unwrap_or(Value::Null)),
        Some(_) => Err(type_error("active.messages?.at is not a function")),
    }
}

/// `availability.employees?.[0]?.canReceive`.
fn first_can_receive(employees: Option<&Value>) -> Option<&Value> {
    let first = match employees {
        Some(Value::Array(items)) => items.first(),
        Some(Value::Object(fields)) => fields.get("0"),
        _ => None,
    };
    prop(first, "canReceive")
}

struct Flag<'a>(&'a AtomicBool);

impl Drop for Flag<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

struct Submitting<'a>(&'a Mutex<HashSet<String>>, String);

impl Drop for Submitting<'_> {
    fn drop(&mut self) {
        self.0.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).remove(&self.1);
    }
}

/// `createTelegramBridge`. 상태는 Node처럼 JSON 값 하나이며 await 동안 잠금을 쥐지 않는다.
pub struct TelegramBridge {
    state: Mutex<Value>,
    persist: Persist,
    telegram: TelegramCall,
    backend: BackendCall,
    logger: Logger,
    checking: AtomicBool,
    submitting: Mutex<HashSet<String>>,
}

impl TelegramBridge {
    pub fn new(mut state: Value, persist: Persist, telegram: TelegramCall, backend: BackendCall, logger: Logger) -> Result<Arc<Self>, String> {
        let Value::Object(fields) = &mut state else {
            return Err(format!("Cannot create property 'chats' on {}", js_string(&state)));
        };
        for (key, default) in [("chats", json!({})), ("pending", json!({})), ("offset", json!(0))] {
            if nullish(fields.get(key)) {
                fields.insert(key.into(), default);
            }
        }
        if !matches!(fields.get("allowedChats"), Some(Value::Object(allowed)) if !allowed.is_empty()) {
            return Err("Explicit Telegram owner allowlist is required".into());
        }
        Ok(Arc::new(Self {
            state: Mutex::new(state),
            persist,
            telegram,
            backend,
            logger,
            checking: AtomicBool::new(false),
            submitting: Mutex::new(HashSet::new()),
        }))
    }

    fn lock(&self) -> MutexGuard<'_, Value> {
        self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// 현재 상태의 사본.
    pub fn state(&self) -> Value {
        self.lock().clone()
    }

    /// 상태 일부만 읽는다(잠금 안에서 f를 부른다).
    pub fn with_state<R>(&self, read: impl FnOnce(&Value) -> R) -> R {
        read(&self.lock())
    }

    #[cfg(test)]
    pub(crate) fn edit_state(&self, edit: impl FnOnce(&mut Value)) {
        edit(&mut self.lock());
    }

    fn save(&self, state: &Value) -> Result<(), CallError> {
        (self.persist)(&js_ordered(state)).map_err(CallError::new)
    }

    fn log(&self, level: LogLevel, line: &str) {
        (self.logger)(level, line);
    }

    /// `remember(chatID, messageID, characterID)`. 답글 경로는 채팅마다 최근 500개만 남긴다.
    fn remember(&self, state: &mut Value, chat: &str, message_id: Option<&Value>, character: &Value) -> Result<(), CallError> {
        if !js_truthy(message_id) {
            return Ok(());
        }
        let routes = object_mut(chat_state(state, chat)?.get_mut("routes"), "routes")?;
        routes.insert(template(message_id), character.clone());
        let excess = routes.len().saturating_sub(MAX_ROUTES);
        let oldest: Vec<String> = js_keys(routes).into_iter().take(excess).map(str::to_string).collect();
        for key in oldest {
            routes.shift_remove(&key);
        }
        self.save(state)
    }

    /// `send(chatID, text, characterID, extra)`. 직원이 정해진 답이면 메시지 번호를 답글 경로로 기억한다.
    async fn send(&self, chat: Option<&Value>, text: Option<Value>, character: Option<&Value>, extra: Option<(&str, Value)>) -> Result<Value, CallError> {
        let mut body = Map::new();
        insert_defined(&mut body, "chat_id", chat);
        insert_defined(&mut body, "text", text.as_ref());
        if let Some((key, value)) = extra {
            body.insert(key.into(), value);
        }
        let message = (self.telegram)("sendMessage", Value::Object(body)).await?;
        if let Some(character) = character.filter(|character| js_truthy(Some(*character))) {
            let message_id = strict_prop(Some(&message), "message_id")?.cloned();
            let mut state = self.lock();
            self.remember(&mut state, &template(chat), message_id.as_ref(), character)?;
        }
        Ok(message)
    }

    async fn say(&self, chat: &str, text: impl Into<String>, character: Option<&Value>) -> Result<Value, CallError> {
        self.send(Some(&json!(chat)), Some(Value::String(text.into())), character, None).await
    }

    /// `characters()`. 응답의 characters ?? [].
    async fn characters(&self) -> Result<Vec<Value>, CallError> {
        let data = (self.backend)("/api/characters", None).await?;
        array_items(strict_prop(Some(&data), "characters")?, "list.find")
    }

    /// `roleplay(characterID)`. 404만 null(롤플레이 없음)로 보고 다른 오류는 그대로 올린다.
    async fn roleplay(&self, character: Option<&Value>) -> Result<Value, CallError> {
        let path = format!("/api/roleplay/profiles/{}", encode_uri_component(&template(character)));
        match (self.backend)(&path, None).await {
            Err(error) if error.status == Some(404) => Ok(Value::Null),
            other => other,
        }
    }

    /// `menu(chatID)`. 직원마다 버튼 하나, 현재 선택에 ✓, 롤플레이가 켜진 직원에 표시를 붙인다.
    pub async fn menu(&self, chat: &str) -> Result<(), CallError> {
        let list = self.characters().await?;
        let mut ids = Vec::with_capacity(list.len());
        for character in &list {
            ids.push(strict_prop(Some(character), "id")?.cloned());
        }
        let modes = try_join_all(ids.iter().map(|id| self.roleplay(id.as_ref()))).await?;
        let selected = chat_state(&mut self.lock(), chat)?.get("selected").cloned();
        let keyboard: Vec<Value> = list
            .iter()
            .zip(&ids)
            .zip(&modes)
            .map(|((character, id), mode)| {
                let check = if strict_equal(id.as_ref(), selected.as_ref()) { "✓ " } else { "" };
                let roleplay = if js_truthy(prop(prop(Some(mode), "profile"), "enabled")) { " · 롤플레이" } else { "" };
                json!([{
                    "text": format!("{check}{}{roleplay}", template(prop(Some(character), "name"))),
                    "callback_data": format!("staff:{}", template(id.as_ref())),
                }])
            })
            .collect();
        let markup = json!({ "inline_keyboard": keyboard });
        self.send(Some(&json!(chat)), Some(json!(MENU_TEXT)), None, Some(("reply_markup", markup))).await?;
        Ok(())
    }

    /// `receive(update)`. 받은 번호를 먼저 저장하므로 전송 결과가 불명확해도 같은 요청을 다시 실행하지 않는다.
    pub async fn receive(&self, update: &Value) -> Result<(), CallError> {
        let Some(update_id) = safe_integer(strict_prop(Some(update), "update_id")?) else {
            return Ok(());
        };
        {
            let mut state = self.lock();
            if update_id < number_of(state.get("offset")) {
                return Ok(());
            }
            state["offset"] = number_value(update_id + 1.0);
            self.save(&state)?;
        }
        let callback = prop(Some(update), "callback_query");
        let message = coalesce(prop(callback, "message"), prop(Some(update), "message"));
        let chat = prop(message, "chat");
        let from = coalesce(prop(callback, "from"), prop(message, "from"));
        let from_id = prop(from, "id");
        if !is_text(prop(chat, "type"), "private") || !js_truthy(from_id) || js_truthy(prop(from, "is_bot")) {
            return Ok(());
        }
        let chat_id = template(prop(chat, "id"));
        {
            let mut state = self.lock();
            let owner = match state.get("allowedChats") {
                Some(Value::Object(allowed)) => allowed.get(&chat_id).map(|owner| template(Some(owner))),
                _ => return Err(type_error("Cannot convert undefined or null to object")),
            };
            if owner != Some(template(from_id)) {
                return Ok(());
            }
            chat_state(&mut state, &chat_id)?;
        }
        if js_truthy(callback) {
            return self.receive_selection(&chat_id, callback).await;
        }
        let Some(text) = optional_trim(prop(message, "text"), "message.text")?.filter(|text| !text.is_empty()) else {
            self.say(&chat_id, "현재는 텍스트 대화를 지원합니다. 내용을 글로 보내주세요.", None).await?;
            return Ok(());
        };
        let command = text.split(is_js_whitespace).next().unwrap_or_default().split('@').next().unwrap_or_default();
        match command {
            "/start" | "/office" | "/staff" | "/help" => return self.menu(&chat_id).await,
            "/new" => return self.start_new_conversation(&chat_id).await,
            "/status" => return self.status(&chat_id).await,
            _ if text.starts_with('/') => return self.menu(&chat_id).await,
            _ => {}
        }
        self.dispatch(&chat_id, update_id, message, &text).await
    }

    /// 직원 선택 버튼(callback_query) 처리.
    async fn receive_selection(&self, chat_id: &str, callback: Option<&Value>) -> Result<(), CallError> {
        let mut body = Map::new();
        insert_defined(&mut body, "callback_query_id", prop(callback, "id"));
        (self.telegram)("answerCallbackQuery", Value::Object(body)).await?;
        let data = coalesce(prop(callback, "data"), None).map_or_else(String::new, js_string);
        let id = data.strip_prefix("staff:").unwrap_or(&data).to_string();
        let id_value = json!(id);
        let list = self.characters().await?;
        let mut found = None;
        for character in &list {
            if strict_equal(strict_prop(Some(character), "id")?, Some(&id_value)) {
                found = Some(character);
                break;
            }
        }
        let Some(character) = found else {
            return self.menu(chat_id).await;
        };
        {
            let mut state = self.lock();
            chat_state(&mut state, chat_id)?.insert("selected".into(), id_value.clone());
            self.save(&state)?;
        }
        let active = self.roleplay(Some(&id_value)).await?;
        let next = if js_truthy(prop(prop(Some(&active), "profile"), "enabled")) {
            "앱에서 진행 중인 롤플레이 대화에 이어집니다."
        } else {
            "이제 메시지를 보내세요."
        };
        let text = format!("{} 선택 완료. {next}", template(prop(Some(character), "name")));
        self.say(chat_id, text, Some(&id_value)).await?;
        Ok(())
    }

    /// `/new`. 롤플레이면 기존 새 대화 API를 부르고, 아니면 이 채팅의 대화 ID만 잊는다(기록은 그대로).
    async fn start_new_conversation(&self, chat_id: &str) -> Result<(), CallError> {
        let selected = chat_state(&mut self.lock(), chat_id)?.get("selected").cloned();
        let active = self.roleplay(selected.as_ref()).await?;
        if js_truthy(prop(prop(Some(&active), "profile"), "enabled")) {
            if js_truthy(prop(Some(&active), "generating")) {
                self.say(chat_id, "롤플레이 답변을 만드는 중입니다. 끝난 후 새 대화를 시작해 주세요.", selected.as_ref()).await?;
                return Ok(());
            }
            let path = format!("/api/roleplay/profiles/{}/chats", encode_uri_component(&template(selected.as_ref())));
            (self.backend)(&path, Some(json!({}))).await?;
            self.say(chat_id, "새 롤플레이 대화를 시작했습니다. 이전 대화 기록과 설정은 보존됩니다.", selected.as_ref()).await?;
            return Ok(());
        }
        let selected = {
            let mut state = self.lock();
            let local = chat_state(&mut state, chat_id)?;
            let selected = local.get("selected").cloned();
            match local.get_mut("conversations") {
                Some(Value::Object(conversations)) => {
                    conversations.shift_remove(&template(selected.as_ref()));
                }
                None | Some(Value::Null) => return Err(type_error("Cannot convert undefined or null to object")),
                Some(_) => {}
            }
            self.save(&state)?;
            selected
        };
        self.say(chat_id, "선택한 직원과 다음 메시지부터 새 대화를 시작합니다. 기존 기록은 보존됩니다.", selected.as_ref()).await?;
        Ok(())
    }

    /// `/status`. 직원마다 롤플레이 여부·접수 가능 여부를 한 줄씩.
    async fn status(&self, chat_id: &str) -> Result<(), CallError> {
        let data = (self.backend)("/api/agent-jobs", None).await?;
        let employees = array_items(strict_prop(Some(&data), "employees")?, "employees.map")?;
        let lines = try_join_all(employees.iter().map(|employee| async move {
            let key = coalesce(strict_prop(Some(employee), "characterId")?, prop(Some(employee), "id"));
            let mode = self.roleplay(key).await?;
            let state = if js_truthy(prop(prop(Some(&mode), "profile"), "enabled")) {
                let generating = if js_truthy(prop(Some(&mode), "generating")) { "답변 생성 중" } else { "대화 가능" };
                format!("롤플레이 · {generating}")
            } else if js_truthy(prop(Some(employee), "canReceive")) {
                "대화 가능".to_string()
            } else {
                format!("작업 중 ({})", template(prop(Some(employee), "state")))
            };
            Ok::<_, CallError>(format!("{}: {state}", template(prop(Some(employee), "name"))))
        }))
        .await?;
        let text = lines.join("\n");
        self.say(chat_id, if text.is_empty() { "직원이 없습니다.".to_string() } else { text }, None).await?;
        Ok(())
    }

    /// 일반 메시지. 답글·이름 접두어·현재 선택 순으로 직원을 정하고 접수 상태를 먼저 저장한 뒤 한 번만 보낸다.
    async fn dispatch(&self, chat_id: &str, update_id: f64, message: Option<&Value>, text: &str) -> Result<(), CallError> {
        let list = self.characters().await?;
        let mut target = {
            let mut state = self.lock();
            let local = chat_state(&mut state, chat_id)?;
            let reply = template(prop(prop(message, "reply_to_message"), "message_id"));
            let route = match local.get("routes") {
                None | Some(Value::Null) => return Err(type_error(format!("Cannot read properties of undefined (reading '{reply}')"))),
                routes => prop(routes, &reply),
            };
            coalesce(route, local.get("selected")).cloned()
        };
        let mut prompt = text.to_string();
        for character in &list {
            let name = template(strict_prop(Some(character), "name")?);
            let id = prop(Some(character), "id");
            for prefix in [format!("{name}:"), format!("{name}："), format!("@{name} "), format!("@{} ", template(id))] {
                if let Some(rest) = text.strip_prefix(prefix.as_str()) {
                    target = id.cloned();
                    prompt = js_trim(rest).to_string();
                }
            }
        }
        let mut found = None;
        for character in &list {
            if strict_equal(strict_prop(Some(character), "id")?, target.as_ref()) {
                found = Some(character);
                break;
            }
        }
        let Some(character) = found.filter(|character| js_truthy(Some(*character))).filter(|_| !prompt.is_empty()) else {
            return self.menu(chat_id).await;
        };
        let name = prop(Some(character), "name").cloned();
        let label = template(name.as_ref());
        let target_text = encode_uri_component(&template(target.as_ref()));
        let active = self.roleplay(target.as_ref()).await?;
        let is_roleplay = matches!(prop(prop(Some(&active), "profile"), "enabled"), Some(Value::Bool(true)));
        if is_roleplay && js_truthy(prop(Some(&active), "generating")) {
            self.say(chat_id, format!("{label}의 롤플레이 답변을 만드는 중입니다. 완료 후 이어 보내주세요."), target.as_ref()).await?;
            return Ok(());
        }
        if !is_roleplay {
            let availability = (self.backend)(&format!("/api/agent-jobs?characterId={target_text}"), None).await?;
            if !js_truthy(first_can_receive(strict_prop(Some(&availability), "employees")?)) {
                let text = format!("{label}은 현재 작업 중입니다. 끝난 후 다시 보내거나 /office에서 다른 직원을 선택하세요.");
                self.say(chat_id, text, target.as_ref()).await?;
                return Ok(());
            }
        }
        {
            let mut state = self.lock();
            let local = chat_state(&mut state, chat_id)?;
            match &target {
                Some(target) => {
                    local.insert("selected".into(), target.clone());
                }
                None => {
                    local.shift_remove("selected");
                }
            }
            self.save(&state)?;
        }
        let mut reply = Map::new();
        insert_defined(&mut reply, "message_id", prop(message, "message_id"));
        let arrow = if is_roleplay { " 롤플레이에" } else { "에게" };
        let status = self
            .send(Some(&json!(chat_id)), Some(json!(format!("⏳ {label}{arrow} 전달합니다."))), target.as_ref(), Some(("reply_parameters", Value::Object(reply))))
            .await?;
        let key = number_to_string(update_id);
        let mut pending = Map::new();
        pending.insert("chatID".into(), json!(chat_id));
        insert_defined(&mut pending, "characterID", target.as_ref());
        insert_defined(&mut pending, "name", name.as_ref());
        insert_defined(&mut pending, "statusMessageID", strict_prop(Some(&status), "message_id")?);
        pending.insert("stage".into(), json!("launching"));
        pending.insert("nextPart".into(), json!(0));
        if is_roleplay {
            pending.insert("mode".into(), json!("roleplay"));
            pending.insert("afterMessageID".into(), last_message_id(prop(Some(&active), "messages"))?);
        }
        let _submitting = {
            let mut state = self.lock();
            pending_map(&mut state)?.insert(key.clone(), Value::Object(pending.clone()));
            self.save(&state)?;
            self.submitting.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).insert(key.clone());
            Submitting(&self.submitting, key.clone())
        };
        let launched: Result<(), CallError> = async {
            if is_roleplay {
                let path = format!("/api/roleplay/profiles/{target_text}/messages");
                let accepted = (self.backend)(&path, Some(json!({ "content": prompt }))).await?;
                let chat = prop(Some(&accepted), "chatId").cloned();
                if !js_truthy(strict_prop(Some(&accepted), "accepted")?) || !js_truthy(chat.as_ref()) {
                    return Err(CallError::new("Invalid roleplay response"));
                }
                pending.insert("roleplayChatID".into(), chat.unwrap_or_default());
                pending.insert("stage".into(), json!("running"));
                let mut state = self.lock();
                write_back(&mut state, &key, &pending);
                return self.save(&state);
            }
            let conversation_key = template(target.as_ref());
            let conversation = {
                let mut state = self.lock();
                match chat_state(&mut state, chat_id)?.get("conversations") {
                    None | Some(Value::Null) => return Err(type_error(format!("Cannot read properties of undefined (reading '{conversation_key}')"))),
                    conversations => prop(conversations, &conversation_key).cloned(),
                }
            };
            let mut body = Map::new();
            insert_defined(&mut body, "characterId", target.as_ref());
            body.insert("requestSource".into(), json!("remote"));
            body.insert("prompt".into(), json!(prompt));
            if js_truthy(conversation.as_ref()) {
                insert_defined(&mut body, "conversationId", conversation.as_ref());
            }
            let job = (self.backend)("/api/agent-jobs", Some(Value::Object(body))).await?;
            let turn = strict_prop(Some(&job), "turnId")?.cloned();
            let conversation = prop(Some(&job), "conversationId").cloned();
            if !js_truthy(turn.as_ref()) || !js_truthy(conversation.as_ref()) {
                return Err(CallError::new("Invalid job response"));
            }
            pending.insert("turnID".into(), turn.unwrap_or_default());
            pending.insert("stage".into(), json!("running"));
            let mut state = self.lock();
            write_back(&mut state, &key, &pending);
            object_mut(chat_state(&mut state, chat_id)?.get_mut("conversations"), "conversations")?
                .insert(conversation_key, conversation.unwrap_or_default());
            self.save(&state)
        }
        .await;
        let Err(error) = launched else {
            return Ok(());
        };
        pending.insert("stage".into(), json!("uncertain"));
        {
            let mut state = self.lock();
            write_back(&mut state, &key, &pending);
            self.save(&state)?;
        }
        let text = if error.status == Some(409) {
            format!("{label}이 다른 작업을 시작해 접수하지 못했습니다. 잠시 후 다시 보내주세요.")
        } else {
            format!("{label} 전달을 확인하지 못했습니다. /status와 앱의 대화 기록을 확인해 주세요. 같은 요청을 자동으로 다시 실행하지 않습니다.")
        };
        self.say(chat_id, text, target.as_ref()).await?;
        self.log(LogLevel::Warn, &format!("Telegram employee submission did not complete {}", error.status_label()));
        let mut state = self.lock();
        pending_map(&mut state)?.shift_remove(&key);
        self.save(&state)
    }

    /// `deliver(key, pending)`. 끝난 업무·롤플레이 답을 조각으로 보내고 상태 메시지를 바꾼다.
    /// 조각마다 보내기 전에 다음 번호를 저장해, 결과가 불명확한 전송은 다시 보내지 않는다.
    async fn deliver(&self, key: &str, mut pending: Map<String, Value>) -> Result<(), CallError> {
        let chat = pending.get("chatID").cloned();
        let character = pending.get("characterID").cloned();
        let name = template(pending.get("name"));
        let stage = pending.get("stage");
        if is_text(stage, "launching") || is_text(stage, "uncertain") {
            let text = format!("{name}에게 전달하던 중 연결이 끊겼습니다. 앱 기록을 확인해 주세요. 같은 요청은 자동 재실행하지 않았습니다.");
            self.send(chat.as_ref(), Some(json!(text)), character.as_ref(), None).await?;
            let mut state = self.lock();
            pending_map(&mut state)?.shift_remove(key);
            return self.save(&state);
        }
        let roleplay = is_text(pending.get("mode"), "roleplay");
        let turn = if roleplay {
            match self.roleplay_turn(&pending, character.as_ref()).await? {
                Some(turn) => turn,
                None => return Ok(()),
            }
        } else {
            let path = format!("/api/live-feed/{}", encode_uri_component(&template(pending.get("turnID"))));
            let data = (self.backend)(&path, None).await?;
            strict_prop(Some(&data), "turn")?.cloned().unwrap_or(Value::Null)
        };
        if !js_truthy(Some(&turn)) {
            return Ok(());
        }
        let status = match prop(Some(&turn), "status") {
            Some(Value::String(status)) if TERMINAL_STATUSES.contains(&status.as_str()) => status.clone(),
            _ => return Ok(()),
        };
        if nullish(pending.get("parts")) {
            let error_message = coalesce(prop(Some(&turn), "errorMessage"), None).map_or_else(String::new, js_string);
            let failure = if error_message.to_ascii_uppercase().contains("SSH") || error_message.contains("4090") || error_message.contains("로컬 연결 실패") {
                "직원의 로컬 모델(4090) 연결에 실패했습니다. /office에서 다른 직원을 선택하거나 앱에서 로컬 모델 연결을 확인해 주세요.".to_string()
            } else {
                format!("작업 {} — 앱에서 상세 내용을 확인해 주세요.", if status == "failed" { "실패" } else { "중단" })
            };
            let response = optional_trim(prop(Some(&turn), "response"), "turn.response")?.filter(|response| !response.is_empty());
            let body = response.unwrap_or_else(|| {
                if status == "completed" { "완료했습니다. 응답 본문은 앱에서 확인해 주세요.".to_string() } else { failure }
            });
            let parts = split_telegram_text(&format!("[{name}]\n{body}"), TEXT_LIMIT);
            pending.insert("parts".into(), json!(parts));
        }
        loop {
            let length = match pending.get("parts") {
                Some(Value::Array(parts)) => parts.len() as f64,
                _ => f64::NAN,
            };
            let next = number_of(pending.get("nextPart"));
            if !(next < length) {
                break;
            }
            let part = match pending.get("parts") {
                Some(Value::Array(parts)) if next >= 0.0 && next.fract() == 0.0 => parts.get(next as usize).cloned(),
                _ => None,
            };
            pending.insert("nextPart".into(), number_value(next + 1.0));
            {
                let mut state = self.lock();
                write_back(&mut state, key, &pending);
                self.save(&state)?;
            }
            if let Err(error) = self.send(chat.as_ref(), part, character.as_ref(), None).await {
                if !error.uncertain {
                    pending.insert("nextPart".into(), number_value(number_of(pending.get("nextPart")) - 1.0));
                } else {
                    pending.insert("uncertainDelivery".into(), json!(true));
                }
                if js_truthy(error.retry_after.as_ref()) {
                    let after = number_of(error.retry_after.as_ref());
                    pending.insert("retryAt".into(), number_value(now_milliseconds() + after * 1000.0));
                }
                if matches!(error.status, Some(400 | 403)) {
                    pending.insert("stage".into(), json!("delivery-failed"));
                }
                let mut state = self.lock();
                write_back(&mut state, key, &pending);
                self.save(&state)?;
                return Err(error);
            }
        }
        let uncertain = js_truthy(pending.get("uncertainDelivery"));
        let needs_input = js_truthy(prop(Some(&turn), "needsInput"));
        let label = if uncertain {
            "답장 일부의 전송 확인 불가 · 앱에서 확인"
        } else if needs_input {
            "답변 필요 · 메시지를 이어 보내세요"
        } else if status == "completed" {
            "완료"
        } else if status == "failed" {
            "실행 실패"
        } else {
            "중단"
        };
        let icon = if uncertain {
            "⚠️"
        } else if status == "failed" {
            "❌"
        } else if status == "interrupted" {
            "⏹️"
        } else if needs_input {
            "❓"
        } else {
            "✅"
        };
        let mut body = Map::new();
        insert_defined(&mut body, "chat_id", chat.as_ref());
        insert_defined(&mut body, "message_id", pending.get("statusMessageID"));
        body.insert("text".into(), json!(format!("{icon} {name}: {label}")));
        let _ = (self.telegram)("editMessageText", Value::Object(body)).await;
        {
            let mut state = self.lock();
            pending_map(&mut state)?.shift_remove(key);
            self.save(&state)?;
        }
        let kind = if roleplay { "roleplay" } else { "turn" };
        let reference = template(coalesce(pending.get("turnID"), pending.get("roleplayChatID")));
        self.log(LogLevel::Log, &format!("Telegram {kind} delivered: {reference}"));
        Ok(())
    }

    /// 롤플레이 답 판정. None은 아직 기다릴 때다. 대화가 바뀌었거나 앱 입력이 겹치면 엉뚱한 답을 보내지 않고 중단으로 끝낸다.
    async fn roleplay_turn(&self, pending: &Map<String, Value>, character: Option<&Value>) -> Result<Option<Value>, CallError> {
        let current = self.roleplay(character).await?;
        if !strict_equal(prop(prop(Some(&current), "profile"), "currentChatId"), pending.get("roleplayChatID")) {
            return Ok(Some(json!({
                "status": "interrupted",
                "response": "앱에서 롤플레이 대화가 변경되어 이전 답변을 여기로 가져오지 않았습니다. 앱에서 이전 대화를 확인해 주세요.",
            })));
        }
        if js_truthy(strict_prop(Some(&current), "generating")?) {
            return Ok(None);
        }
        if js_truthy(prop(Some(&current), "lastError")) {
            return Ok(Some(json!({
                "status": "failed",
                "response": "롤플레이 답변 생성에 실패했습니다. 앱의 롤플레이 대화에 표시된 오류를 확인해 주세요.",
            })));
        }
        let history = array_items(prop(Some(&current), "messages"), "history.slice(...).filter")?;
        let after = pending.get("afterMessageID");
        let mut anchor: isize = -1;
        if js_truthy(after) {
            for (index, message) in history.iter().enumerate() {
                if strict_equal(strict_prop(Some(message), "id")?, after) {
                    anchor = index as isize;
                    break;
                }
            }
        }
        let added = &history[(anchor + 1) as usize..];
        let mut roles = Vec::with_capacity(added.len());
        for message in added {
            roles.push(strict_prop(Some(message), "role")?);
        }
        let users = roles.iter().filter(|role| is_text(**role, "user")).count();
        if (js_truthy(after) && anchor < 0) || users > 1 {
            return Ok(Some(json!({
                "status": "interrupted",
                "response": "앱과 텔레그램의 동시 입력으로 답변 대상을 확정할 수 없습니다. 앱에서 롤플레이 대화를 확인해 주세요.",
            })));
        }
        let Some(user) = roles.iter().position(|role| is_text(*role, "user")) else {
            return Ok(None);
        };
        let reply = added[user + 1..].iter().zip(&roles[user + 1..]).find(|(_, role)| is_text(**role, "char")).map(|(message, _)| message);
        match reply.filter(|reply| js_truthy(Some(*reply))) {
            Some(reply) => Ok(Some(json!({ "status": "completed", "response": prop(Some(reply), "content") }))),
            None => Ok(None),
        }
    }

    /// `tick()`. 겹쳐 돌지 않으며, 접수 중인 항목·배달 실패·재시도 대기 항목은 건너뛴다.
    pub async fn tick(&self) -> Result<(), CallError> {
        if self.checking.swap(true, Ordering::SeqCst) {
            return Ok(());
        }
        let _checking = Flag(&self.checking);
        let entries: Vec<(String, Value)> = {
            let state = self.lock();
            match state.get("pending") {
                Some(Value::Object(pending)) => js_keys(pending).into_iter().map(|key| (key.to_string(), pending[key].clone())).collect(),
                _ => Vec::new(),
            }
        };
        for (key, snapshot) in entries {
            if self.submitting.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).contains(&key) {
                continue;
            }
            let current = self.lock().get("pending").and_then(|pending| pending.get(&key)).cloned();
            let pending = match current.unwrap_or(snapshot) {
                Value::Object(pending) => pending,
                Value::Null => return Err(type_error("Cannot read properties of null (reading 'stage')")),
                _ => {
                    self.log(LogLevel::Warn, "Telegram delivery pending connection");
                    continue;
                }
            };
            if is_text(pending.get("stage"), "delivery-failed") || number_of(pending.get("retryAt")) > now_milliseconds() {
                continue;
            }
            if let Err(error) = self.deliver(&key, pending).await {
                self.log(LogLevel::Warn, &format!("Telegram delivery pending {}", error.status_label()));
            }
        }
        Ok(())
    }
}
