// Telegram 연결을 혼자 도는 프로세스로 띄운다. Node scripts/office-telegram.mjs의 잠금·상태 파일·소유자 등록·긴 폴링 순서를 따른다.

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{Map, Value, json};

use super::{
    BACKEND_URL, CallError, LogLevel, Logger, Persist, TELEGRAM_API_URL, TelegramBridge, TelegramCall, backend_client, coalesce,
    console_logger, insert_defined, is_text, js_ordered, number_of, number_value, prop, read_telegram_configuration, strict_prop,
    telegram_client, template, type_error,
};
use crate::js::{js_keys, js_max, js_string, js_to_number, js_truthy, join_path, node_fs_message, now_milliseconds, number_to_string, parse_js_json, string_to_number, to_js_json, uv_error};

const FALLBACK_TEXT: &str = "연결 문제로 요청 처리를 확인하지 못했습니다. /status와 앱 기록을 확인한 뒤 다시 보내주세요.";

/// 실행 설정. 시험은 임시 폴더·가짜 서버 주소·짧은 간격을 넣는다.
pub struct CliOptions {
    /// 상태·잠금 파일 폴더(~/.config/officestra).
    pub directory: String,
    /// 토큰 파일(~/.config/officestra/telegram.env).
    pub configuration_file: String,
    pub telegram_api_url: String,
    pub backend_url: String,
    /// process.argv. `--pair`·`--announce`가 들어 있는지만 본다.
    pub arguments: Vec<String>,
    /// setInterval(tick, 2000)
    pub tick_interval: Duration,
    /// 폴링 실패 뒤 최소 대기(5000ms)
    pub poll_retry_floor: Duration,
    /// SIGTERM·SIGINT를 받으면 잠금을 지우고 0으로 끝낸다(실행 파일만 켠다).
    pub handle_signals: bool,
    pub logger: Logger,
}

impl CliOptions {
    /// 실제 실행 값. Node와 같은 폴더·토큰 파일·4317 주소를 쓴다.
    pub fn from_environment() -> Self {
        let home = crate::local_artifacts::home_directory();
        Self {
            directory: join_path(&[&home, ".config/officestra"]),
            configuration_file: join_path(&[&home, ".config/officestra/telegram.env"]),
            telegram_api_url: TELEGRAM_API_URL.into(),
            backend_url: BACKEND_URL.into(),
            arguments: std::env::args().collect(),
            tick_interval: Duration::from_millis(2_000),
            poll_retry_floor: Duration::from_millis(5_000),
            handle_signals: true,
            logger: console_logger(),
        }
    }
}

/// 잡은 잠금 파일. Node의 process.on('exit') 처리처럼 어떤 길로 끝나든 지운다.
struct LockFile(String);

impl Drop for LockFile {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

struct AbortOnDrop(tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

fn fatal(logger: &Logger, message: &str) -> i32 {
    logger(LogLevel::Error, &format!("Error: {message}"));
    1
}

fn fs_message(error: &io::Error, syscall: &str, path: &str) -> String {
    match error.raw_os_error() {
        Some(libc::EEXIST) => format!("EEXIST: file already exists, {syscall} '{path}'"),
        _ => node_fs_message(error, syscall, path),
    }
}

/// readFileSync 실패 문구. 폴더면 경로 없이 read다.
fn read_message(error: &io::Error, path: &str) -> String {
    match error.kind() {
        io::ErrorKind::IsADirectory => "EISDIR: illegal operation on a directory, read".into(),
        _ => fs_message(error, "open", path),
    }
}

/// JS ToInt32(`value | 0`).
fn to_int32(number: f64) -> i32 {
    if !number.is_finite() {
        return 0;
    }
    let wrapped = number.trunc().rem_euclid(4_294_967_296.0);
    (wrapped as u32) as i32
}

/// 잠금 파일을 'wx'로 만든다. 이미 있으면 적힌 pid에 kill(pid, 0)을 보내 살아 있으면 Ok(false),
/// 없는 프로세스(ESRCH)면 지우고 다시 만든다. 그 밖의 경우는 Node처럼 실패로 끝낸다.
fn acquire_lock(path: &str) -> Result<bool, String> {
    let open = || OpenOptions::new().write(true).create_new(true).mode(0o600).open(path);
    let mut file = match open() {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            let bytes = fs::read(path).map_err(|error| read_message(&error, path))?;
            let pid = string_to_number(&String::from_utf8_lossy(&bytes));
            // process.kill은 `pid != (pid | 0)`이면 인자 오류다(빈 파일은 0이라 통과한다).
            if pid != f64::from(to_int32(pid)) {
                return Err(format!("The \"pid\" argument must be of type number. Received type number ({})", number_to_string(pid)));
            }
            // SAFETY: 신호 0은 프로세스 존재와 권한만 확인하고 아무것도 보내지 않는다.
            if unsafe { libc::kill(to_int32(pid), 0) } == 0 {
                return Ok(false);
            }
            let error = io::Error::last_os_error();
            if error.raw_os_error() != Some(libc::ESRCH) {
                return Err(format!("kill {}", uv_error(&error).0));
            }
            fs::remove_file(path).map_err(|error| fs_message(&error, "unlink", path))?;
            open().map_err(|error| fs_message(&error, "open", path))?
        }
        Err(error) => return Err(fs_message(&error, "open", path)),
    };
    file.write_all(std::process::id().to_string().as_bytes()).map_err(|error| {
        let (code, description) = uv_error(&error);
        format!("{code}: {description}, write")
    })?;
    Ok(true)
}

/// `persist(state)`. 임시 파일(0600)에 JSON 한 줄을 쓰고 이름을 바꿔 덮는다.
pub fn state_writer(path: String) -> Persist {
    Arc::new(move |state: &Value| {
        let temporary = format!("{path}.tmp");
        let mut bytes = to_js_json(state).map_err(|error| error.to_string())?;
        bytes.push(b'\n');
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&temporary)
            .map_err(|error| fs_message(&error, "open", &temporary))?;
        file.write_all(&bytes).map_err(|error| {
            let (code, description) = uv_error(&error);
            format!("{code}: {description}, write")
        })?;
        drop(file);
        fs::rename(&temporary, &path).map_err(|error| {
            let (code, description) = uv_error(&error);
            format!("{code}: {description}, rename '{temporary}' -> '{path}'")
        })
    })
}

/// `office-telegram.mjs` 전체. 돌려주는 값이 종료 코드다.
pub async fn run(options: CliOptions) -> i32 {
    let logger = options.logger.clone();
    if let Err(error) = fs::DirBuilder::new().recursive(true).mode(0o700).create(&options.directory) {
        return fatal(&logger, &fs_message(&error, "mkdir", &options.directory));
    }
    let state_path = join_path(&[&options.directory, "telegram-state.json"]);
    let lock_path = join_path(&[&options.directory, "telegram-bridge.pid"]);
    match acquire_lock(&lock_path) {
        Ok(true) => {}
        Ok(false) => {
            logger(LogLevel::Error, "Telegram bridge already running");
            return 1;
        }
        Err(message) => return fatal(&logger, &message),
    }
    let _lock = LockFile(lock_path);
    let body = serve(&options, &state_path);
    if !options.handle_signals {
        return body.await;
    }
    use tokio::signal::unix::{SignalKind, signal};
    let (Ok(mut terminate), Ok(mut interrupt)) = (signal(SignalKind::terminate()), signal(SignalKind::interrupt())) else {
        return body.await;
    };
    tokio::select! {
        code = body => code,
        _ = terminate.recv() => 0,
        _ = interrupt.recv() => 0,
    }
}

async fn serve(options: &CliOptions, state_path: &str) -> i32 {
    let logger = &options.logger;
    let token = match read_telegram_configuration(&options.configuration_file) {
        Ok(token) => token,
        Err(message) => return fatal(logger, &message),
    };
    let telegram = match telegram_client(&token, &options.telegram_api_url) {
        Ok(telegram) => telegram,
        Err(message) => return fatal(logger, &message),
    };
    drop(token);
    let backend = match backend_client(&options.backend_url) {
        Ok(backend) => backend,
        Err(message) => return fatal(logger, &message),
    };
    let persist = state_writer(state_path.to_string());
    if options.arguments.iter().any(|argument| argument == "--pair") {
        return match pair(&telegram, &persist, state_path).await {
            Ok(()) => {
                logger(LogLevel::Log, "Verified private Telegram owner paired");
                0
            }
            Err(message) => fatal(logger, &message),
        };
    }
    if !Path::new(state_path).exists() {
        return fatal(logger, "Pair the verified owner before starting Telegram bridge");
    }
    let state = match fs::read(state_path) {
        Ok(bytes) => match parse_js_json(&String::from_utf8_lossy(&bytes)) {
            Ok(state) => state,
            Err(error) => {
                logger(LogLevel::Error, &format!("SyntaxError: {error}"));
                return 1;
            }
        },
        Err(error) => return fatal(logger, &read_message(&error, state_path)),
    };
    let info = match telegram("getWebhookInfo", json!({})).await {
        Ok(info) => info,
        Err(error) => return fatal(logger, &error.message),
    };
    match strict_prop(Some(&info), "url") {
        Err(error) => return fatal(logger, &error.message),
        Ok(url) if js_truthy(url) => return fatal(logger, "Existing Telegram webhook detected; refusing to replace it"),
        Ok(_) => {}
    }
    let bridge = match TelegramBridge::new(state, persist, telegram.clone(), backend, logger.clone()) {
        Ok(bridge) => bridge,
        Err(message) => return fatal(logger, &message),
    };
    if options.arguments.iter().any(|argument| argument == "--announce") {
        return match announce(&bridge, &telegram).await {
            Ok(()) => {
                logger(LogLevel::Log, "Telegram employee menu sent");
                0
            }
            Err(error) => fatal(logger, &error.message),
        };
    }
    poll(bridge, telegram, options).await
}

/// JS `value + 1`. 문자열은 이어 붙인 뒤 Math.max가 숫자로 바꾼다.
fn plus_one(value: Option<&Value>) -> f64 {
    match value {
        None | Some(Value::Object(_)) => f64::NAN,
        Some(Value::Null) => 1.0,
        Some(Value::Bool(flag)) => f64::from(u8::from(*flag)) + 1.0,
        Some(Value::Number(number)) => number.as_f64().unwrap_or(f64::NAN) + 1.0,
        Some(other) => string_to_number(&format!("{}1", js_string(other))),
    }
}

/// `--pair`. 최근 2시간 안의 개인 대화 /start 보낸 사람이 정확히 한 명일 때만 소유자로 등록한다. 기존 상태는 덮지 않는다.
async fn pair(telegram: &TelegramCall, persist: &Persist, state_path: &str) -> Result<(), String> {
    if Path::new(state_path).exists() {
        return Err("Telegram state already exists; owner pairing is never overwritten".into());
    }
    let updates = telegram("getUpdates", json!({ "timeout": 0, "limit": 100 })).await.map_err(|error| error.message)?;
    let Value::Array(updates) = updates else {
        return Err("updates.filter is not a function".into());
    };
    let now = now_milliseconds() / 1000.0;
    let mut recent = Vec::new();
    for update in &updates {
        let message = strict_prop(Some(update), "message").map_err(|error| error.message)?;
        if is_text(prop(prop(message, "chat"), "type"), "private")
            && !js_truthy(prop(prop(message, "from"), "is_bot"))
            && is_text(prop(message, "text"), "/start")
            && now - number_of(prop(message, "date")) < 7200.0
        {
            recent.push(message);
        }
    }
    let mut owners: Vec<String> = Vec::new();
    for message in &recent {
        let from = strict_prop(*message, "from").map_err(|error| error.message)?;
        let owner = template(strict_prop(from, "id").map_err(|error| error.message)?);
        if !owners.contains(&owner) {
            owners.push(owner);
        }
    }
    let (true, Some(message)) = (owners.len() == 1, recent.last()) else {
        return Err("Exactly one recent verified owner /start is required".into());
    };
    let offset = updates.iter().fold(0.0, |largest, update| js_max(largest, plus_one(prop(Some(update), "update_id"))));
    let mut allowed = Map::new();
    allowed.insert(template(prop(prop(*message, "chat"), "id")), json!(owners[0]));
    let state = json!({ "offset": number_value(offset), "allowedChats": allowed, "chats": {}, "pending": {} });
    persist(&js_ordered(&state))
}

/// `--announce`. 명령 목록을 등록하고 허용된 채팅마다 직원 메뉴를 보낸다.
async fn announce(bridge: &TelegramBridge, telegram: &TelegramCall) -> Result<(), CallError> {
    let commands = json!({ "commands": [
        { "command": "office", "description": "대화할 직원 선택" },
        { "command": "status", "description": "직원별 현재 작업 상태" },
        { "command": "new", "description": "선택한 직원과 새 대화" },
    ] });
    telegram("setMyCommands", commands).await?;
    let chats: Vec<String> = bridge.with_state(|state| match state.get("allowedChats") {
        Some(Value::Object(allowed)) => js_keys(allowed).into_iter().map(str::to_string).collect(),
        _ => Vec::new(),
    });
    for chat in chats {
        bridge.menu(&chat).await?;
    }
    Ok(())
}

/// 받은 업데이트를 차례로 처리한다. 하나가 실패하면 소유자에게 확인 안내를 보내고 다음으로 넘어간다.
async fn handle_updates(bridge: &TelegramBridge, telegram: &TelegramCall, logger: &Logger, updates: Value) -> Result<(), CallError> {
    let updates = match updates {
        Value::Array(updates) => updates,
        // for...of가 글자마다 receive를 부르지만 update_id가 없어 아무 일도 없다.
        Value::String(_) => return Ok(()),
        _ => return Err(type_error("updates is not iterable")),
    };
    for update in &updates {
        let Err(error) = bridge.receive(update).await else {
            continue;
        };
        logger(LogLevel::Error, &format!("Telegram update handling failed {}", error.status_label()));
        if update.is_null() {
            return Err(type_error("Cannot read properties of null (reading 'callback_query')"));
        }
        let callback = prop(Some(update), "callback_query");
        let message = coalesce(prop(callback, "message"), prop(Some(update), "message"));
        let from = coalesce(prop(callback, "from"), prop(message, "from"));
        let chat_id = prop(prop(message, "chat"), "id");
        if is_text(prop(prop(message, "chat"), "type"), "private")
            && js_truthy(prop(from, "id"))
            && bridge.with_state(|state| template(prop(state.get("allowedChats"), &template(chat_id)))) == template(prop(from, "id"))
        {
            let mut body = Map::new();
            insert_defined(&mut body, "chat_id", chat_id);
            body.insert("text".into(), json!(FALLBACK_TEXT));
            let _ = telegram("sendMessage", Value::Object(body)).await;
        }
    }
    Ok(())
}

/// 긴 폴링과 2초 점검. 401·409(토큰 무효·다른 폴링)는 0으로 끝내고, 그 밖의 실패는 max(5초, retry_after) 뒤 다시 한다.
/// 점검(tick)이 던진 오류는 Node의 처리 안 된 rejection처럼 1로 끝낸다.
async fn poll(bridge: Arc<TelegramBridge>, telegram: TelegramCall, options: &CliOptions) -> i32 {
    let logger = &options.logger;
    let (fatal_sender, mut fatal_receiver) = tokio::sync::mpsc::unbounded_channel::<String>();
    let ticker = {
        let bridge = bridge.clone();
        let period = options.tick_interval;
        tokio::spawn(async move {
            let mut interval = tokio::time::interval_at(tokio::time::Instant::now() + period, period);
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                interval.tick().await;
                if let Err(error) = bridge.tick().await {
                    let _ = fatal_sender.send(error.message);
                    return;
                }
            }
        })
    };
    let _ticker = AbortOnDrop(ticker);
    logger(LogLevel::Log, "OFFICESTRA Telegram bridge started");
    let polling = async {
        loop {
            let mut body = Map::new();
            insert_defined(&mut body, "offset", bridge.with_state(|state| state.get("offset").cloned()).as_ref());
            body.insert("timeout".into(), json!(25));
            body.insert("allowed_updates".into(), json!(["message", "callback_query"]));
            let failure = match telegram("getUpdates", Value::Object(body)).await {
                Ok(updates) => match handle_updates(&bridge, &telegram, logger, updates).await {
                    Ok(()) => continue,
                    Err(error) => error,
                },
                Err(error) => error,
            };
            logger(LogLevel::Error, &format!("Telegram polling failed {}", failure.status_label()));
            if matches!(failure.status, Some(401 | 409)) {
                return 0;
            }
            let retry = coalesce(failure.retry_after.as_ref(), None).map_or(0.0, js_to_number);
            let delay = js_max(options.poll_retry_floor.as_secs_f64() * 1000.0, retry * 1000.0);
            // setTimeout은 1ms 미만·NaN·2^31-1ms 초과를 1ms로 바꾼다.
            let delay = if (1.0..=2_147_483_647.0).contains(&delay) { delay } else { 1.0 };
            tokio::time::sleep(Duration::from_secs_f64(delay / 1000.0)).await;
        }
    };
    tokio::select! {
        code = polling => code,
        Some(message) = fatal_receiver.recv() => fatal(logger, &message),
    }
}
