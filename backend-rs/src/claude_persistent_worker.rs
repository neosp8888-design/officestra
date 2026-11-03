// 직원마다 Claude Code 프로세스 하나를 띄워 두고 stream-json 입력으로 턴을 이어 받는 지속 세션. Node claude-persistent-worker.mjs와 같다.

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Map, Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{ChildStderr, ChildStdin, ChildStdout};
use tokio::sync::{oneshot, watch};

use crate::agent_runtime::launch::Environment;
use crate::agent_runtime::process_group::{ProcessGroup, detached_command};
use crate::child_process::spawn_error_message;
use crate::claude_cost_state::read_claude_cost_state;
use crate::js::{js_keys, js_string, js_string_or_empty, js_to_number, js_trim, now_milliseconds, number_to_string, parse_js_json, to_js_json, utf16_suffix, uv_error};
use crate::local_embedding::signal_name;

pub const DEFAULT_SUGGESTION_GRACE: Duration = Duration::from_secs(3);
/// 중단 제어 요청을 보내면 Claude Code는 보통 수십 ms 안에 비용이 담긴 result를 돌려준다. 이 안에 오지 않으면 그대로 끝낸다.
pub const DEFAULT_INTERRUPT_RESULT_TIMEOUT: Duration = Duration::from_secs(3);
const FORCE_KILL_DELAY: Duration = Duration::from_secs(5);
const STDERR_TAIL_UNITS: usize = 8_192;
const COMPACT_PROMPT: &str = "/compact OFFICESTRA 직원 이름과 직급 호칭을 쓰지 말고, 사용자는 '사용자'로 표현하며 요청·결정·진행 상태만 내용 중심으로 요약해 주세요.";
const CLOSED: &str = "Claude Code 지속 세션이 종료됐습니다.";
pub const DEFAULT_CLOSE_REASON: &str = "Claude Code 지속 세션을 종료했습니다.";
const CANCELLED: &str = "사용자가 Claude Code 업무를 중단했습니다.";
const MODEL_USAGE_FIELDS: [&str; 10] = [
    "inputTokens",
    "input_tokens",
    "outputTokens",
    "output_tokens",
    "cacheReadInputTokens",
    "cache_read_input_tokens",
    "cacheCreationInputTokens",
    "cache_creation_input_tokens",
    "costUSD",
    "cost_usd",
];

static NEXT_WORKER_ID: AtomicU64 = AtomicU64::new(1);

/// 줄 하나를 받아 처리하는 비동기 함수. 실패하면 지속 세션 전체가 실패한다.
pub type LineHandler = Box<dyn FnMut(String) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send>> + Send>;

pub struct WorkerOptions {
    pub executable: String,
    pub arguments: Vec<String>,
    pub cwd: Option<String>,
    pub env: Environment,
    pub signature: String,
    pub session_id: Option<String>,
    pub resume_transcript_path: Option<String>,
    /// 프로세스가 끝났거나 닫혔을 때 한 번 부른다. 받은 값은 작업자 ID다. 실행기 쪽 잠금을 쥔 채로 close를 부르지 않는다.
    pub on_exit: Box<dyn FnOnce(u64) + Send>,
    pub suggestion_grace: Duration,
    pub interrupt_result_timeout: Duration,
}

/// Claude 결과의 세션 누적 비용과 모델별 누적 사용량. 다음 결과에서 이 값을 빼 턴 증분을 만든다.
#[derive(Clone, Debug, PartialEq)]
pub struct CumulativeUsage {
    pub total_cost_usd: Option<f64>,
    pub model_usage: Map<String, Value>,
}

#[derive(Debug, PartialEq)]
pub struct CompactTokens {
    pub pre_tokens: Option<f64>,
    pub post_tokens: Option<f64>,
}

struct Current {
    turn: u64,
    on_line: Arc<tokio::sync::Mutex<LineHandler>>,
    done: oneshot::Sender<Result<(), String>>,
    /// 값은 보내지 않는다. 턴이 어떻게든 끝나 Current가 버려지면 구독자의 changed()가 끝난다.
    settled: watch::Sender<()>,
    result_seen: bool,
    suggestion_timer: Option<tokio::task::AbortHandle>,
}

struct State {
    session_id: Option<String>,
    current: Option<Current>,
    next_turn: u64,
    closed: bool,
    terminal_error: Option<String>,
    stderr_tail: String,
    cumulative: Option<CumulativeUsage>,
    resumed_without_cost_baseline: bool,
    on_exit: Option<Box<dyn FnOnce(u64) + Send>>,
}

pub struct ClaudePersistentWorker {
    pub id: u64,
    pub signature: String,
    pub cwd: Option<String>,
    suggestion_grace: Duration,
    interrupt_result_timeout: Duration,
    process: ProcessGroup,
    stdin: tokio::sync::Mutex<Option<ChildStdin>>,
    state: Mutex<State>,
}

impl ClaudePersistentWorker {
    /// 프로세스를 띄운다. 띄우지 못하면 Node의 'error' 이벤트처럼 곧바로 실패한 작업자가 된다.
    pub fn spawn(options: WorkerOptions) -> Arc<Self> {
        let cumulative = read_claude_cost_state(options.resume_transcript_path.as_deref(), options.session_id.as_deref())
            .map(|state| CumulativeUsage {
                total_cost_usd: Some(state.total_cost_usd),
                model_usage: state.model_usage.as_object().cloned().unwrap_or_default(),
            });
        let resumed_without_cost_baseline = options.session_id.as_deref().is_some_and(|id| !id.is_empty()) && cumulative.is_none();
        let spawned = detached_command(&options.executable, &options.arguments, options.cwd.as_deref(), &options.env)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn();
        let (mut child, spawn_error) = match spawned {
            Ok(child) => (Some(child), None),
            Err(error) => (None, Some(spawn_error_message(&options.executable, &error))),
        };
        let worker = Arc::new(Self {
            id: NEXT_WORKER_ID.fetch_add(1, Ordering::Relaxed),
            signature: options.signature,
            cwd: options.cwd,
            suggestion_grace: options.suggestion_grace,
            interrupt_result_timeout: options.interrupt_result_timeout,
            process: ProcessGroup::new(child.as_ref().and_then(tokio::process::Child::id)),
            stdin: tokio::sync::Mutex::new(child.as_mut().and_then(|child| child.stdin.take())),
            state: Mutex::new(State {
                session_id: options.session_id,
                current: None,
                next_turn: 0,
                closed: spawn_error.is_some(),
                terminal_error: spawn_error,
                stderr_tail: String::new(),
                cumulative,
                resumed_without_cost_baseline,
                on_exit: Some(options.on_exit),
            }),
        });
        match child {
            Some(mut child) => {
                let (stdout, stderr) = (child.stdout.take(), child.stderr.take());
                let supervisor = Arc::clone(&worker);
                tokio::spawn(async move {
                    let errors = tokio::spawn(collect_stderr(Arc::clone(&supervisor), stderr));
                    supervisor.read_output(stdout).await;
                    let status = child.wait().await;
                    let _ = errors.await;
                    match status {
                        Ok(status) => {
                            supervisor.process.record_exit(&status);
                            supervisor.on_close(status.code(), std::os::unix::process::ExitStatusExt::signal(&status));
                        }
                        Err(error) => supervisor.fail(error.to_string()),
                    }
                });
            }
            None => {
                // 실행 실패 문구는 바로 남겨 첫 턴도 그 문구로 거절한다. Node는 이를 다음 틱의 'error' 이벤트로 알리므로,
                // 종료 알림은 실행기가 작업자를 등록한 뒤에 보낸다.
                let failed = Arc::clone(&worker);
                tokio::spawn(async move { failed.notify_exit() });
            }
        }
        worker
    }

    pub fn pid(&self) -> Option<i32> {
        self.process.pid()
    }

    pub fn session_id(&self) -> Option<String> {
        self.lock().session_id.clone()
    }

    /// matches. 닫히지 않았고 설정 서명과 세션 ID(빈 값은 없음)가 같아야 다시 쓴다.
    pub fn matches(&self, signature: &str, session_id: Option<&str>) -> bool {
        let state = self.lock();
        if state.closed || state.terminal_error.is_some() || self.signature != signature {
            return false;
        }
        let normalize = |id: Option<&str>| Some(js_trim(id.unwrap_or("")).to_string()).filter(|id| !id.is_empty());
        normalize(state.session_id.as_deref()) == normalize(session_id)
    }

    /// runTurn. 사용자 메시지 한 줄을 보내고, result 뒤 추천 문장까지 받거나 유예 시간이 지나면 끝난다.
    pub async fn run_turn<F, Fut>(self: &Arc<Self>, prompt: &str, mut on_line: F) -> Result<(), String>
    where
        F: FnMut(String) -> Fut + Send + 'static,
        Fut: Future<Output = Result<(), String>> + Send + 'static,
    {
        let handler: LineHandler = Box::new(move |line| Box::pin(on_line(line)));
        let receiver = {
            let mut state = self.lock();
            if state.closed || state.terminal_error.is_some() {
                return Err(state.terminal_error.clone().unwrap_or_else(|| CLOSED.into()));
            }
            if state.current.is_some() {
                return Err("Claude Code 지속 세션이 이전 업무를 아직 처리 중입니다.".into());
            }
            let (done, receiver) = oneshot::channel();
            state.next_turn += 1;
            state.current = Some(Current {
                turn: state.next_turn,
                on_line: Arc::new(tokio::sync::Mutex::new(handler)),
                done,
                settled: watch::Sender::new(()),
                result_seen: false,
                suggestion_timer: None,
            });
            receiver
        };
        let message = json!({
            "type": "user",
            "message": { "role": "user", "content": prompt },
            "parent_tool_use_id": null,
        });
        if let Err(error) = self.write_line(&message).await {
            self.fail(error);
        }
        receiver.await.unwrap_or_else(|_| Err(CLOSED.into()))
    }

    /// compact. `/compact`를 한 턴으로 보내고 완료 경계의 압축 전후 토큰을 돌려준다.
    pub async fn compact(self: &Arc<Self>) -> Result<CompactTokens, String> {
        let seen = Arc::new(Mutex::new((None::<Value>, None::<String>)));
        let recorder = Arc::clone(&seen);
        self.run_turn(COMPACT_PROMPT, move |line| {
            if let Ok(event) = parse_js_json(&line) {
                let mut seen = recorder.lock().unwrap_or_else(|poison| poison.into_inner());
                let text = |key: &str| event.get(key).and_then(Value::as_str);
                if text("type") == Some("system") && text("subtype") == Some("compact_boundary") {
                    seen.0 = Some(event.clone());
                }
                if text("type") == Some("result") && (event.get("is_error") == Some(&Value::Bool(true)) || text("subtype") != Some("success")) {
                    let message = present(event.get("result")).or_else(|| present(event.get("error")));
                    seen.1 = Some(message.map_or_else(|| "Claude Code 컨텍스트 압축이 실패했습니다.".into(), js_string));
                }
            }
            std::future::ready(Ok(()))
        })
        .await?;
        let (boundary, result_error) = std::mem::take(&mut *seen.lock().unwrap_or_else(|poison| poison.into_inner()));
        if let Some(message) = result_error.filter(|message| !message.is_empty()) {
            return Err(message);
        }
        let boundary = boundary.ok_or("Claude Code가 압축 완료 경계를 보고하지 않았습니다.")?;
        let empty = Value::Object(Map::new());
        let metadata = present(boundary.get("compact_metadata")).or_else(|| present(boundary.get("compactMetadata"))).unwrap_or(&empty);
        let tokens = |snake: &str, camel: &str| finite_token_count(present(metadata.get(snake)).or_else(|| present(metadata.get(camel))));
        Ok(CompactTokens { pre_tokens: tokens("pre_tokens", "preTokens"), post_tokens: tokens("post_tokens", "postTokens") })
    }

    /// cancelCurrent. 바로 끊으면 result가 오지 않아 비용이 비므로, interrupt 제어 요청을 먼저 보내고
    /// 비용이 담긴 result가 오거나 제한 시간이 지나면 닫는다.
    pub async fn cancel_current(self: &Arc<Self>) {
        let settled = {
            let state = self.lock();
            state.current.as_ref().filter(|_| !state.closed).map(|current| current.settled.subscribe())
        };
        if let Some(mut settled) = settled
            && self.request_interrupt().await
        {
            let _ = tokio::time::timeout(self.interrupt_result_timeout, settled.changed()).await;
        }
        self.close(CANCELLED);
    }

    async fn request_interrupt(&self) -> bool {
        let request = json!({
            "type": "control_request",
            "request_id": format!("interrupt-{}", number_to_string(now_milliseconds().floor())),
            "request": { "subtype": "interrupt" },
        });
        self.write_line(&request).await.is_ok()
    }

    async fn write_line(&self, value: &Value) -> Result<(), String> {
        let mut line = to_js_json(value).map_err(|error| error.to_string())?;
        line.push(b'\n');
        let mut stdin = self.stdin.lock().await;
        let pipe = stdin.as_mut().ok_or("Claude Code 지속 세션의 입력 스트림이 닫혔습니다.")?;
        let written = async {
            pipe.write_all(&line).await?;
            pipe.flush().await
        };
        written.await.map_err(|error| format!("write {}", uv_error(&error).0))
    }

    /// close. 진행 중인 턴을 이 이유로 실패시키고, 입력을 닫은 뒤 프로세스 그룹을 끝낸다.
    pub fn close(&self, reason: &str) {
        {
            let mut state = self.lock();
            if state.closed {
                return;
            }
            state.closed = true;
            reject_current(&mut state, reason);
        }
        // 이미 쓰는 중이면 신호로 끝내므로 입력 닫기는 건너뛴다.
        if let Ok(mut stdin) = self.stdin.try_lock() {
            stdin.take();
        }
        self.process.terminate(FORCE_KILL_DELAY, true);
        self.notify_exit();
    }

    fn fail(&self, error: String) {
        {
            let mut state = self.lock();
            if state.terminal_error.is_some() {
                return;
            }
            state.terminal_error = Some(error.clone());
            state.closed = true;
            reject_current(&mut state, &error);
        }
        self.process.terminate(FORCE_KILL_DELAY, true);
        self.notify_exit();
    }

    fn on_close(&self, code: Option<i32>, signal: Option<i32>) {
        let message = {
            let state = self.lock();
            if state.closed {
                None
            } else {
                let diagnostic = js_trim(&state.stderr_tail);
                let suffix = if diagnostic.is_empty() { String::new() } else { format!(" {diagnostic}") };
                Some(match signal {
                    Some(signal) => format!("Claude Code 지속 세션이 {} 신호로 종료됐습니다.{suffix}", signal_name(signal)),
                    None => format!(
                        "Claude Code 지속 세션이 종료 코드 {}로 끝났습니다.{suffix}",
                        code.map_or_else(|| "unknown".into(), |code| code.to_string())
                    ),
                })
            }
        };
        match message {
            Some(message) => self.fail(message),
            None => self.notify_exit(),
        }
    }

    fn notify_exit(&self) {
        let on_exit = self.lock().on_exit.take();
        if let Some(on_exit) = on_exit {
            on_exit(self.id);
        }
    }

    async fn read_output(self: &Arc<Self>, stdout: Option<ChildStdout>) {
        let Some(stdout) = stdout else {
            return;
        };
        let mut reader = BufReader::new(stdout);
        let mut buffer = Vec::new();
        loop {
            buffer.clear();
            match reader.read_until(b'\n', &mut buffer).await {
                Ok(0) => return,
                Ok(_) => {}
                Err(error) => return self.fail(error.to_string()),
            }
            let line = buffer.strip_suffix(b"\n").unwrap_or(&buffer);
            let line = line.strip_suffix(b"\r").unwrap_or(line);
            if let Err(error) = self.handle_line(&String::from_utf8_lossy(line)).await {
                return self.fail(error);
            }
        }
    }

    async fn handle_line(self: &Arc<Self>, line: &str) -> Result<(), String> {
        let Ok(mut object) = parse_js_json(line) else {
            return Ok(());
        };
        if object.is_null() {
            // Node는 null 줄에서 `object.type`을 읽다 예외가 나 지속 세션이 실패한다.
            return Err("Cannot read properties of null (reading 'type')".into());
        }
        let kind = object.get("type").and_then(Value::as_str).map(str::to_string);
        let current = {
            let mut state = self.lock();
            if kind.as_deref() == Some("system") && object.get("subtype").and_then(Value::as_str) == Some("init") {
                let session_id = js_string_or_empty(object.get("session_id"));
                let session_id = js_trim(&session_id);
                if !session_id.is_empty() {
                    state.session_id = Some(session_id.to_string());
                }
            }
            if kind.as_deref() == Some("result") {
                object = turn_scoped_result(&mut state, &object);
            }
            state.current.as_ref().map(|current| (current.turn, Arc::clone(&current.on_line), current.result_seen))
        };
        let Some((turn, on_line, result_seen)) = current else {
            return Ok(());
        };
        // 정상 추천은 그 턴의 result 뒤에 온다. 이전 턴 유예 시간을 넘겨 늦게 온 추천은 다음 턴에 붙이지 않는다.
        if kind.as_deref() == Some("prompt_suggestion") && !result_seen {
            return Ok(());
        }
        let text = String::from_utf8(to_js_json(&object).map_err(|error| error.to_string())?).unwrap_or_default();
        let handled = (on_line.lock().await)(text).await;
        handled?;
        if kind.as_deref() == Some("prompt_suggestion") {
            self.finish_current(turn);
            return Ok(());
        }
        if kind.as_deref() != Some("result") {
            return Ok(());
        }
        let failed = object.get("is_error") == Some(&Value::Bool(true)) || object.get("subtype").and_then(Value::as_str) != Some("success");
        {
            let mut state = self.lock();
            let Some(current) = state.current.as_mut().filter(|current| current.turn == turn) else {
                return Ok(());
            };
            current.result_seen = true;
            if !failed {
                let worker = Arc::clone(self);
                let grace = self.suggestion_grace;
                current.suggestion_timer = Some(
                    tokio::spawn(async move {
                        tokio::time::sleep(grace).await;
                        worker.finish_current(turn);
                    })
                    .abort_handle(),
                );
                return Ok(());
            }
        }
        self.finish_current(turn);
        Ok(())
    }

    fn finish_current(&self, turn: u64) {
        let mut state = self.lock();
        if state.current.as_ref().is_none_or(|current| current.turn != turn) {
            return;
        }
        if let Some(current) = state.current.take() {
            if let Some(timer) = &current.suggestion_timer {
                timer.abort();
            }
            let _ = current.done.send(Ok(()));
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|poison| poison.into_inner())
    }
}

fn reject_current(state: &mut State, error: &str) {
    if let Some(current) = state.current.take() {
        if let Some(timer) = &current.suggestion_timer {
            timer.abort();
        }
        let _ = current.done.send(Err(error.to_string()));
    }
}

async fn collect_stderr(worker: Arc<ClaudePersistentWorker>, stderr: Option<ChildStderr>) {
    let Some(mut stderr) = stderr else {
        return;
    };
    let mut buffer = [0_u8; 8_192];
    while let Ok(count) = stderr.read(&mut buffer).await {
        if count == 0 {
            return;
        }
        let mut state = worker.lock();
        let joined = format!("{}{}", state.stderr_tail, String::from_utf8_lossy(&buffer[..count]));
        state.stderr_tail = utf16_suffix(&joined, STDERR_TAIL_UNITS);
    }
}

/// turnScopedResult. 누적 결과를 턴 증분으로 바꾼다. 재개했는데 저장된 누적 기준이 없으면 첫 결과의 비용은 모르는 값으로 둔다.
fn turn_scoped_result(state: &mut State, result: &Value) -> Value {
    let (scoped, cumulative) = scoped_claude_cumulative_result(result, state.cumulative.as_ref());
    state.cumulative = Some(cumulative);
    if !state.resumed_without_cost_baseline {
        return scoped;
    }
    state.resumed_without_cost_baseline = false;
    // 첫 재개 누적값에는 이전 턴이 섞일 수 있다. 사용량은 두고 비용과 모델별 누적은 지운다.
    let mut safe = scoped;
    if let Some(object) = safe.as_object_mut() {
        object.insert("total_cost_usd".into(), Value::Null);
        object.remove("modelUsage");
        object.remove("model_usage");
    }
    safe
}

/// scopedClaudeCumulativeResult. 같은 프로세스의 result는 세션 누적이므로 이전 누적을 빼 이 턴만 남긴다.
/// 누적 비용이 줄면 새 query가 시작된 것으로 보고 그대로 쓴다.
pub fn scoped_claude_cumulative_result(result: &Value, previous: Option<&CumulativeUsage>) -> (Value, CumulativeUsage) {
    let usable = |key: &str| matches!(result.get(key), Some(Value::Object(_) | Value::Array(_)));
    let model_usage_key = ["modelUsage", "model_usage"].into_iter().find(|key| usable(key));
    let cumulative = CumulativeUsage {
        total_cost_usd: finite_number(result.get("total_cost_usd")),
        model_usage: model_usage_key
            .map(|key| entries(&result[key]).into_iter().map(|(model, usage)| (model, usage.clone())).collect())
            .unwrap_or_default(),
    };
    let Some(previous) = previous else {
        return (result.clone(), cumulative);
    };
    let reset = matches!((cumulative.total_cost_usd, previous.total_cost_usd), (Some(current), Some(prior)) if current < prior);
    let mut scoped = result.clone();
    let Some(object) = scoped.as_object_mut() else {
        return (scoped, cumulative);
    };
    if !reset && let Some(total) = cumulative.total_cost_usd {
        object.insert("total_cost_usd".into(), Value::from(cumulative_delta(total, previous.total_cost_usd)));
    }
    if let Some(key) = model_usage_key {
        let models = entries(&result[key])
            .into_iter()
            .map(|(model, usage)| {
                let prior = if reset { None } else { previous.model_usage.get(&model) };
                let scoped = scoped_model_usage(usage, prior);
                (model, scoped)
            })
            .collect();
        object.insert(key.into(), Value::Object(models));
    }
    (scoped, cumulative)
}

/// Object.entries. 배열이면 번호가 키다.
fn entries(value: &Value) -> Vec<(String, &Value)> {
    match value {
        Value::Object(object) => js_keys(object).into_iter().map(|key| (key.to_string(), &object[key])).collect(),
        Value::Array(items) => items.iter().enumerate().map(|(index, value)| (index.to_string(), value)).collect(),
        _ => Vec::new(),
    }
}

fn scoped_model_usage(usage: &Value, previous: Option<&Value>) -> Value {
    let Value::Object(fields) = usage else {
        return usage.clone();
    };
    let mut result = fields.clone();
    for field in MODEL_USAGE_FIELDS {
        let Some(value) = finite_number(fields.get(field)) else {
            continue;
        };
        let prior = finite_number(previous.and_then(|previous| previous.get(field)));
        result.insert(field.into(), Value::from(cumulative_delta(value, prior)));
    }
    Value::Object(result)
}

fn cumulative_delta(value: f64, previous: Option<f64>) -> f64 {
    match previous {
        Some(previous) if value >= previous => value - previous,
        _ => value,
    }
}

/// finiteNumber. null·없음·빈 글자는 없음이고, Number()가 유한한 0 이상이어야 한다.
fn finite_number(value: Option<&Value>) -> Option<f64> {
    let value = value.filter(|value| !value.is_null() && value.as_str() != Some(""))?;
    let number = js_to_number(value);
    (number.is_finite() && number >= 0.0).then_some(number)
}

/// finiteTokenCount. 유한한 0 이상 숫자만 반올림해 받는다.
fn finite_token_count(value: Option<&Value>) -> Option<f64> {
    value.and_then(Value::as_f64).filter(|count| count.is_finite() && *count >= 0.0).map(f64::round)
}

/// JS `??`처럼 없음·null이 아닌 값.
fn present(value: Option<&Value>) -> Option<&Value> {
    value.filter(|value| !value.is_null())
}

#[cfg(test)]
mod tests;
