// 4090 모델 앞의 인증된 루프백 다리. Node local-inference-bridge.mjs의 LocalInferenceBridge를 옮겼다.
// 요청은 하나씩만 받고 사용량을 정규화·감사한다. 메모리 한도·측정 실패·유휴·취소 때 소유 자원을 한 번만 내린다.

use std::io;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::task::{Context, Poll};
use std::time::Duration;

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::response::Response;
use futures_util::future::{BoxFuture, FutureExt, Shared};
use futures_util::stream::{Stream, StreamExt};
use serde_json::{Value, json};
use tokio::sync::{mpsc, oneshot};

use super::host::ResourceSample;
use super::requests::{apply_local_reasoning, normalize_llama_messages_request, normalize_local_message_request, normalize_local_responses_request};
use super::tools::{ToolOptions, adapt_local_responses_tools};
use super::usage::{
    LLAMA_MESSAGES_PROFILE, LLAMA_RESPONSES_PROFILE, ResponsesUsageSseTransform, StreamTransform, UsageNormalizer, UsageRecord, UsageSseTransform,
    normalize_responses_usage, safe_nonnegative_integer, verified_responses_profile,
};
use super::{AbortSignal, LocalError};
use crate::js::{js_truthy, now_milliseconds, parse_js_json, to_js_json};

/// 긴 로컬 문맥은 몇 분이 걸릴 수 있다. 0은 벽시계 제한이 없다는 뜻이고, 취소·연결 끊김·자원 감시·SSH 실패는 그대로 요청을 멈춘다.
pub const LOCAL_INFERENCE_REQUEST_TIMEOUT: Duration = Duration::ZERO;
pub const ANTHROPIC_USAGE_PROTOCOL: &str = "anthropic-normalized-v1";
pub const RESPONSES_USAGE_PROTOCOL: &str = "openai-responses-v1";
const MAX_SAFE_INTEGER: u64 = (1 << 53) - 1;
/// Node closeAllConnections는 바로 끊는다. axum은 진행 중 응답을 마칠 때까지 기다리므로 멈춘 받는 쪽에 묶이지 않게 상한을 둔다.
const SERVER_CLOSE_WAIT: Duration = Duration::from_secs(2);

pub type SampleReader = Arc<dyn Fn() -> BoxFuture<'static, Result<ResourceSample, LocalError>> + Send + Sync>;
pub type ReleaseHook = Arc<dyn Fn(String) -> BoxFuture<'static, Result<(), LocalError>> + Send + Sync>;
/// 동기 감사. Node는 Promise를 돌려주면 거부했지만 Rust는 형으로 막는다.
pub type AuditHook = Arc<dyn Fn(UsageRecord) + Send + Sync>;
pub type ObserveHook = Arc<dyn Fn(&Value) + Send + Sync>;
pub type CompletedHook = Arc<dyn Fn() + Send + Sync>;

#[derive(Clone)]
pub struct BridgeOptions {
    pub upstream: String,
    pub token: String,
    pub model: String,
    pub usage_profile: String,
    pub usage_protocol: String,
    pub reasoning: String,
    pub read_resources: SampleReader,
    pub audit: AuditHook,
    pub observe_request: ObserveHook,
    pub on_response_completed: CompletedHook,
    pub release: ReleaseHook,
    pub port: u16,
    pub guard_percent: f64,
    pub vram_guard_percent: f64,
    pub ram_guard_percent: f64,
    pub sample_max_age: Duration,
    pub sample_timeout: Duration,
    pub poll: Duration,
    pub idle: Duration,
    pub request_timeout: Duration,
    pub max_body_bytes: usize,
    pub context_window: Option<u64>,
    pub max_output_tokens: u64,
    /// Node는 10초 고정이다. 시험만 줄인다.
    pub cleanup_timeout: Duration,
}

impl BridgeOptions {
    /// Node 생성자 기본값. 세 메모리 한도는 guardPercent(77)를 따른다.
    pub fn new(upstream: impl Into<String>, token: impl Into<String>, model: impl Into<String>, usage_profile: impl Into<String>, read_resources: SampleReader) -> Self {
        Self {
            upstream: upstream.into(),
            token: token.into(),
            model: model.into(),
            usage_profile: usage_profile.into(),
            usage_protocol: ANTHROPIC_USAGE_PROTOCOL.into(),
            reasoning: "default".into(),
            read_resources,
            audit: Arc::new(|_| {}),
            observe_request: Arc::new(|_| {}),
            on_response_completed: Arc::new(|| {}),
            release: Arc::new(|_| Box::pin(async { Ok(()) })),
            port: 0,
            guard_percent: 77.0,
            vram_guard_percent: 77.0,
            ram_guard_percent: 77.0,
            sample_max_age: Duration::from_millis(5000),
            sample_timeout: Duration::from_millis(3000),
            poll: Duration::from_millis(1000),
            idle: Duration::from_millis(180_000),
            request_timeout: LOCAL_INFERENCE_REQUEST_TIMEOUT,
            max_body_bytes: 8 * 1024 * 1024,
            context_window: None,
            max_output_tokens: 4096,
            cleanup_timeout: Duration::from_secs(10),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BridgeState {
    Idle,
    Starting,
    Ready,
    Stopping,
    Stopped,
    Fault,
}

impl BridgeState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Starting => "starting",
            Self::Ready => "ready",
            Self::Stopping => "stopping",
            Self::Stopped => "stopped",
            Self::Fault => "fault",
        }
    }
}

/// Node `bridge.status`.
#[derive(Debug, Clone, PartialEq)]
pub struct BridgeStatus {
    pub state: BridgeState,
    pub active: bool,
    pub address: Option<String>,
    pub failure: Option<String>,
    pub cleanup_complete: bool,
}

/// 서비스가 쓰는 다리 모양. 시험은 가짜를 넣는다.
pub trait InferenceBridge: Send + Sync {
    /// 시작 중·준비됨이면 같은 시작을 돌려준다.
    fn start(&self) -> BoxFuture<'static, Result<String, LocalError>>;
    /// 처음 부른 이유로 한 번만 멈춘다.
    fn stop(&self, reason: &str) -> BoxFuture<'static, ()>;
    fn status(&self) -> BridgeStatus;
}

pub type BridgeFactory = Arc<dyn Fn(BridgeOptions) -> Result<Arc<dyn InferenceBridge>, String> + Send + Sync>;

pub fn default_bridge_factory() -> BridgeFactory {
    Arc::new(|options| Ok(Arc::new(LocalInferenceBridge::new(options)?) as Arc<dyn InferenceBridge>))
}

/// timingSafeEqual. 길이가 다르면 바로 거짓이다.
pub(crate) fn safe_equal(left: &[u8], right: &[u8]) -> bool {
    left.len() == right.len() && left.iter().zip(right).fold(0u8, |difference, (a, b)| difference | (a ^ b)) == 0
}

fn aborted() -> LocalError {
    LocalError::plain("This operation was aborted")
}

fn code(status: u16) -> StatusCode {
    StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY)
}

fn bytes_response(status: u16, content_type: HeaderValue, body: Vec<u8>) -> Response {
    let mut response = Response::new(Body::from(body));
    *response.status_mut() = code(status);
    let headers = response.headers_mut();
    headers.insert(header::CONTENT_TYPE, content_type);
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

fn json_response(status: u16, value: &Value) -> Response {
    bytes_response(status, HeaderValue::from_static("application/json"), to_js_json(value).unwrap_or_default())
}

fn reply_response(status: u16, message: &str) -> Response {
    json_response(status, &json!({ "error": message }))
}

struct ActiveRequest {
    id: u64,
    abort: AbortSignal,
}

/// 루프백 HTTP 서버 하나. 다리와 서비스 앞문이 같이 쓴다.
pub(super) struct BridgeServer {
    shutdown: oneshot::Sender<()>,
    task: tokio::task::JoinHandle<()>,
}

impl BridgeServer {
    pub(super) fn spawn(listener: tokio::net::TcpListener, router: Router) -> Self {
        let (shutdown, signal) = oneshot::channel::<()>();
        let task = tokio::spawn(async move {
            let _ = axum::serve(listener, router)
                .with_graceful_shutdown(async move {
                    let _ = signal.await;
                })
                .await;
        });
        Self { shutdown, task }
    }

    pub(super) async fn close(self) {
        let _ = self.shutdown.send(());
        let mut task = self.task;
        if tokio::time::timeout(SERVER_CLOSE_WAIT, &mut task).await.is_err() {
            task.abort();
        }
    }
}

struct BridgeShared {
    phase: BridgeState,
    failure: Option<String>,
    address: Option<String>,
    cleanup_complete: bool,
    starting: Option<Shared<BoxFuture<'static, Result<String, LocalError>>>>,
    stopping: Option<Shared<BoxFuture<'static, ()>>>,
    server: Option<BridgeServer>,
    monitor: Option<AbortSignal>,
    /// clearTimeout 대신 세대 번호가 바뀌면 유휴 타이머가 아무것도 하지 않는다.
    idle_generation: u64,
    active: Option<ActiveRequest>,
    next_request: u64,
}

struct BridgeInner {
    options: BridgeOptions,
    upstream: url::Url,
    http: reqwest::Client,
    state: Mutex<BridgeShared>,
}

pub struct LocalInferenceBridge(Arc<BridgeInner>);

impl LocalInferenceBridge {
    /// Node 생성자 검사. 메시지가 같다.
    pub fn new(options: BridgeOptions) -> Result<Self, String> {
        let upstream = url::Url::parse(&options.upstream)
            .ok()
            .filter(|url| {
                url.scheme() == "http"
                    && matches!(url.host_str(), Some("127.0.0.1" | "[::1]"))
                    && url.port().is_some()
                    && url.username().is_empty()
                    && url.password().is_none()
                    && url.path() == "/"
                    && url.query().is_none_or(str::is_empty)
                    && url.fragment().is_none_or(str::is_empty)
            })
            .ok_or("Upstream must be a loopback URL")?;
        let unsafe_text = |text: &str| text.contains(['\r', '\n', '\0']);
        if options.token.len() < 24 || unsafe_text(&options.token) {
            return Err("Dedicated bridge token of at least 24 bytes required".into());
        }
        if options.model.is_empty() || unsafe_text(&options.model) {
            return Err("Pinned local model is required".into());
        }
        let guard = |value: f64, max: f64| value.is_finite() && value > 0.0 && value <= max;
        if !guard(options.guard_percent, 95.0) || !guard(options.ram_guard_percent, 95.0) || !guard(options.vram_guard_percent, 98.0) {
            return Err("Invalid port or memory guard".into());
        }
        let durations = [options.sample_max_age, options.sample_timeout, options.poll, options.idle];
        if durations.iter().any(Duration::is_zero) || options.max_body_bytes == 0 {
            return Err("Invalid limits".into());
        }
        let window_ok = options.context_window.is_none_or(|window| (1..=MAX_SAFE_INTEGER).contains(&window));
        let output_ok = (1..=MAX_SAFE_INTEGER).contains(&options.max_output_tokens) && options.context_window.is_none_or(|window| options.max_output_tokens <= window);
        if !window_ok || !output_ok {
            return Err("Invalid output budget".into());
        }
        let responses = match options.usage_protocol.as_str() {
            ANTHROPIC_USAGE_PROTOCOL => false,
            RESPONSES_USAGE_PROTOCOL => true,
            _ => return Err("Unsupported local usage protocol".into()),
        };
        if responses {
            if !verified_responses_profile(&options.usage_profile) {
                return Err("Explicit verified Responses usage profile required".into());
            }
            normalize_local_responses_request(&json!({}), &options.reasoning, &options.usage_profile, options.context_window, Some(&options.model)).map_err(|error| error.message)?;
        } else {
            UsageNormalizer::new(&options.usage_profile, None)?;
            if options.usage_profile == LLAMA_MESSAGES_PROFILE {
                normalize_llama_messages_request(&json!({}), &options.reasoning, Some(&options.model)).map_err(|error| error.message)?;
            } else {
                apply_local_reasoning(&json!({}), &options.reasoning).map_err(|error| error.message)?;
            }
        }
        let http = crate::http_client_builder().no_proxy().build().map_err(|error| error.to_string())?;
        Ok(Self(Arc::new(BridgeInner {
            options,
            upstream,
            http,
            state: Mutex::new(BridgeShared {
                phase: BridgeState::Idle,
                failure: None,
                address: None,
                cleanup_complete: false,
                starting: None,
                stopping: None,
                server: None,
                monitor: None,
                idle_generation: 0,
                active: None,
                next_request: 0,
            }),
        })))
    }
}

impl InferenceBridge for LocalInferenceBridge {
    fn start(&self) -> BoxFuture<'static, Result<String, LocalError>> {
        start_bridge(&self.0)
    }

    fn stop(&self, reason: &str) -> BoxFuture<'static, ()> {
        stop_bridge(&self.0, reason).boxed()
    }

    fn status(&self) -> BridgeStatus {
        let state = self.0.state.lock().unwrap();
        BridgeStatus {
            state: state.phase,
            active: state.active.is_some(),
            address: state.address.clone(),
            failure: state.failure.clone(),
            cleanup_complete: state.cleanup_complete,
        }
    }
}

impl BridgeInner {
    fn phase(&self) -> BridgeState {
        self.state.lock().unwrap().phase
    }

    /// #resources. 측정이 늦거나 오래됐거나 범위를 벗어나면 503, 한도를 넘으면 503 'Memory budget guard'다.
    async fn resources(&self) -> Result<ResourceSample, LocalError> {
        let sample = match tokio::time::timeout(self.options.sample_timeout, (self.options.read_resources)()).await {
            Err(_) => return Err(LocalError::status(503, "Resource sampler timed out")),
            Ok(sample) => sample?,
        };
        let (vram, ram, sampled_at, now) = (sample.vram_pct(), sample.ram_pct(), sample.sampled_at(), now_milliseconds());
        let percent = |value: f64| (0.0..=100.0).contains(&value);
        if ![vram, ram, sampled_at].iter().all(|value| value.is_finite())
            || !percent(vram)
            || !percent(ram)
            || now - sampled_at > self.options.sample_max_age.as_millis() as f64
            || sampled_at > now + 1000.0
        {
            return Err(LocalError::status(503, "Resource sample unavailable or stale"));
        }
        if vram >= self.options.vram_guard_percent || ram >= self.options.ram_guard_percent {
            return Err(LocalError::status(503, "Memory budget guard"));
        }
        Ok(sample)
    }
}

fn start_bridge(inner: &Arc<BridgeInner>) -> BoxFuture<'static, Result<String, LocalError>> {
    let mut state = inner.state.lock().unwrap();
    match state.phase {
        BridgeState::Starting | BridgeState::Ready => {
            if let Some(starting) = &state.starting {
                return starting.clone().boxed();
            }
        }
        BridgeState::Idle => {}
        _ => return async { Err(LocalError::status(409, "Bridge cannot be restarted")) }.boxed(),
    }
    state.phase = BridgeState::Starting;
    let task_inner = Arc::clone(inner);
    let handle = tokio::spawn(async move {
        match start_steps(&task_inner).await {
            Ok(address) => Ok(address),
            Err(error) => {
                stop_bridge(&task_inner, &error.message).await;
                Err(error)
            }
        }
    });
    let shared = async move { handle.await.unwrap_or_else(|error| Err(LocalError::plain(error.to_string()))) }.boxed().shared();
    state.starting = Some(shared.clone());
    shared.boxed()
}

fn listen_error(error: &io::Error, port: u16) -> LocalError {
    if error.kind() == io::ErrorKind::AddrInUse {
        return LocalError::plain(format!("listen EADDRINUSE: address already in use 127.0.0.1:{port}"));
    }
    LocalError::plain(format!("listen {error}"))
}

async fn start_steps(inner: &Arc<BridgeInner>) -> Result<String, LocalError> {
    let sample = inner.resources().await?;
    if sample.busy_flag() {
        return Err(LocalError::status(503, "GPU reserved by another workload"));
    }
    let cancelled = || LocalError::status(503, "Startup cancelled");
    if inner.phase() != BridgeState::Starting {
        return Err(cancelled());
    }
    let port = inner.options.port;
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port)).await.map_err(|error| listen_error(&error, port))?;
    let bound = listener.local_addr().map_err(|error| LocalError::plain(error.to_string()))?;
    let monitor = AbortSignal::new();
    let address = {
        let mut state = inner.state.lock().unwrap();
        // 듣기 시작 전에 멈췄으면 늦은 듣기 소켓을 남기지 않는다(listener가 여기서 닫힌다).
        if state.phase != BridgeState::Starting {
            return Err(cancelled());
        }
        let address = format!("http://127.0.0.1:{}", bound.port());
        state.server = Some(BridgeServer::spawn(listener, Router::new().fallback(handle).with_state(Arc::downgrade(inner))));
        state.address = Some(address.clone());
        state.phase = BridgeState::Ready;
        state.monitor = Some(monitor.clone());
        address
    };
    spawn_monitor(inner, monitor);
    arm_idle(inner);
    Ok(address)
}

/// 측정 감시. 한도·측정 실패면 그 이유로 멈춘다. 바쁜 GPU는 다른 작업의 것이라 멈추지 않는다.
fn spawn_monitor(inner: &Arc<BridgeInner>, stop: AbortSignal) {
    let (weak, period) = (Arc::downgrade(inner), inner.options.poll);
    tokio::spawn(async move {
        let mut ticks = tokio::time::interval_at(tokio::time::Instant::now() + period, period);
        ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                () = stop.aborted() => return,
                _ = ticks.tick() => {}
            }
            let Some(inner) = weak.upgrade() else {
                return;
            };
            if let Err(error) = inner.resources().await {
                stop_bridge(&inner, &error.message).await;
                return;
            }
        }
    });
}

fn arm_idle(inner: &Arc<BridgeInner>) {
    let mut state = inner.state.lock().unwrap();
    state.idle_generation += 1;
    if state.phase != BridgeState::Ready || state.active.is_some() {
        return;
    }
    let (generation, weak, idle) = (state.idle_generation, Arc::downgrade(inner), inner.options.idle);
    tokio::spawn(async move {
        tokio::time::sleep(idle).await;
        let Some(inner) = weak.upgrade() else {
            return;
        };
        let stopping = {
            let mut state = inner.state.lock().unwrap();
            (state.idle_generation == generation).then(|| begin_stop(&inner, &mut state, "idle"))
        };
        if let Some(stopping) = stopping {
            stopping.await;
        }
    });
}

fn stop_bridge(inner: &Arc<BridgeInner>, reason: &str) -> Shared<BoxFuture<'static, ()>> {
    let mut state = inner.state.lock().unwrap();
    begin_stop(inner, &mut state, reason)
}

/// stop(reason). 듣기를 닫고 진행 중 요청을 중단한 뒤 release를 10초 안에 확인한다. 늦은 확인도 cleanupComplete로 남긴다.
fn begin_stop(inner: &Arc<BridgeInner>, state: &mut BridgeShared, reason: &str) -> Shared<BoxFuture<'static, ()>> {
    if let Some(stopping) = &state.stopping {
        return stopping.clone();
    }
    state.phase = BridgeState::Stopping;
    state.failure = (!matches!(reason, "manual" | "idle")).then(|| reason.to_string());
    if let Some(monitor) = state.monitor.take() {
        monitor.abort();
    }
    state.idle_generation += 1;
    if let Some(active) = &state.active {
        active.abort.abort();
    }
    let server = state.server.take();
    let (task_inner, reason) = (Arc::clone(inner), reason.to_string());
    let handle = tokio::spawn(async move {
        if let Some(server) = server {
            server.close().await;
        }
        let release = (task_inner.options.release)(reason);
        let release_inner = Arc::clone(&task_inner);
        let released = tokio::spawn(async move {
            let done = release.await.is_ok();
            if done {
                release_inner.state.lock().unwrap().cleanup_complete = true;
            }
            done
        });
        let confirmed = matches!(tokio::time::timeout(task_inner.options.cleanup_timeout, released).await, Ok(Ok(true)));
        let mut state = task_inner.state.lock().unwrap();
        if !confirmed {
            state.failure = Some("Owned resource cleanup failed".into());
        }
        state.address = None;
        state.phase = if state.failure.is_some() { BridgeState::Fault } else { BridgeState::Stopped };
    });
    let shared = async move {
        let _ = handle.await;
    }
    .boxed()
    .shared();
    state.stopping = Some(shared.clone());
    shared
}

/// 응답이 끝나기 전에 받는 쪽이 사라지면(Node res 'close') 완료 전 요청을 중단한다.
pub(super) struct ClientGuard {
    pub(super) abort: AbortSignal,
    pub(super) completed: Arc<AtomicBool>,
    pub(super) finished: bool,
}

impl Drop for ClientGuard {
    fn drop(&mut self) {
        if !self.finished && !self.completed.load(Ordering::SeqCst) {
            self.abort.abort();
        }
    }
}

/// 흘려 보내는 응답 본문. 보내는 쪽이 닫히면 끝이고, destroy면 오류로 연결을 끊는다.
pub(super) struct ClientBody {
    pub(super) guard: ClientGuard,
    pub(super) receiver: mpsc::Receiver<Bytes>,
    pub(super) destroyed: Arc<AtomicBool>,
    pub(super) done: bool,
}

impl Stream for ClientBody {
    type Item = Result<Bytes, io::Error>;

    fn poll_next(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = &mut *self;
        if this.done {
            return Poll::Ready(None);
        }
        match this.receiver.poll_recv(context) {
            Poll::Ready(Some(bytes)) => Poll::Ready(Some(Ok(bytes))),
            Poll::Ready(None) => {
                this.done = true;
                if this.destroyed.load(Ordering::SeqCst) {
                    return Poll::Ready(Some(Err(io::Error::other("Local response destroyed"))));
                }
                this.guard.finished = true;
                Poll::Ready(None)
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

pub(super) enum Head {
    Full(Response),
    Stream { status: StatusCode, content_type: HeaderValue, receiver: mpsc::Receiver<Bytes> },
}

/// 요청 작업 쪽 응답 쓰개. Node res의 headersSent·writableEnded·destroy를 흉내 낸다.
pub(super) struct Exchange {
    pub(super) head: Option<oneshot::Sender<Head>>,
    pub(super) body: Option<mpsc::Sender<Bytes>>,
    pub(super) ended: bool,
    pub(super) abort: AbortSignal,
    pub(super) completed: Arc<AtomicBool>,
    pub(super) destroyed: Arc<AtomicBool>,
}

impl Exchange {
    /// res.destroy(). 머리를 보낸 뒤라면 받는 쪽이 정상 끝으로 읽지 않게 표시하고 닫는다.
    pub(super) fn destroy(&mut self) {
        self.ended = true;
        self.head = None;
        if let Some(body) = self.body.take() {
            self.destroyed.store(true, Ordering::SeqCst);
            drop(body);
        }
    }

    /// reply(status, message). 이미 끝났으면 아무것도 안 하고, 머리를 보냈으면 연결을 끊는다.
    pub(super) fn reply(&mut self, status: u16, message: &str) {
        if self.ended {
            return;
        }
        self.ended = true;
        if let Some(head) = self.head.take() {
            let _ = head.send(Head::Full(reply_response(status, message)));
        } else if let Some(body) = self.body.take() {
            // 받는 쪽이 정상 끝으로 읽지 않게 표시를 먼저 세운 뒤 닫는다.
            self.destroyed.store(true, Ordering::SeqCst);
            drop(body);
        }
    }

    fn send_full(&mut self, status: u16, content_type: HeaderValue, body: Vec<u8>) {
        self.ended = true;
        if let Some(head) = self.head.take() {
            let _ = head.send(Head::Full(bytes_response(status, content_type, body)));
        }
    }

    pub(super) fn write_head(&mut self, status: u16, content_type: HeaderValue) {
        let (sender, receiver) = mpsc::channel(16);
        if let Some(head) = self.head.take() {
            let _ = head.send(Head::Stream { status: code(status), content_type, receiver });
        }
        self.body = Some(sender);
    }

    pub(super) async fn write(&mut self, bytes: Bytes) -> Result<(), LocalError> {
        if bytes.is_empty() {
            return Ok(());
        }
        let sent = match &self.body {
            Some(body) => body.send(bytes).await.is_ok(),
            None => false,
        };
        if sent {
            return Ok(());
        }
        if !self.completed.load(Ordering::SeqCst) {
            self.abort.abort();
        }
        Err(LocalError::plain("Premature close"))
    }

    pub(super) fn end(&mut self) {
        self.ended = true;
        self.body = None;
    }

    /// 다리를 멈추기 전에 응답을 닫아 서버 종료가 이 연결을 기다리지 않게 한다.
    pub(super) fn close(&mut self) {
        self.ended = true;
        self.head = None;
        self.body = None;
    }
}

struct Inbound {
    url: String,
    route: String,
    headers: HeaderMap,
    body: Body,
}

async fn handle(State(inner): State<Weak<BridgeInner>>, request: Request) -> Response {
    let Some(inner) = inner.upgrade() else {
        return reply_response(503, "Bridge unavailable");
    };
    let (parts, body) = request.into_parts();
    let token = inner.options.token.as_bytes();
    let header_text = |name: &str| parts.headers.get(name).and_then(|value| value.to_str().ok());
    let bearer = [b"Bearer ".as_slice(), token].concat();
    let authorized = header_text("authorization").is_some_and(|value| safe_equal(value.as_bytes(), &bearer))
        || header_text("x-api-key").is_some_and(|value| safe_equal(value.as_bytes(), token));
    if parts.headers.get(header::ORIGIN).is_some_and(|origin| !origin.is_empty()) || !authorized {
        return reply_response(401, "Unauthorized");
    }
    let url = parts.uri.path_and_query().map_or("/", |path| path.as_str()).to_string();
    let options = &inner.options;
    if parts.method == Method::GET && url == "/health" {
        let state = inner.state.lock().unwrap();
        return json_response(
            200,
            &json!({
                "state": state.phase.as_str(), "active": state.active.is_some(), "usageProtocol": options.usage_protocol,
                "guardPercent": options.guard_percent, "vramGuardPercent": options.vram_guard_percent, "ramGuardPercent": options.ram_guard_percent,
            }),
        );
    }
    // 설치된 Claude SDK가 붙이는 ?beta=true만 허용한다. 임의 질의·우회·호출자가 고른 상류 주소는 받지 않는다.
    let route = url.strip_suffix("?beta=true").unwrap_or(&url).to_string();
    let supported = if options.usage_protocol == RESPONSES_USAGE_PROTOCOL {
        url == "/v1/responses"
    } else {
        route == "/v1/messages" || route == "/v1/messages/count_tokens"
    };
    if parts.method != Method::POST || !supported {
        return reply_response(404, "Unsupported route");
    }
    let (id, abort) = {
        let mut state = inner.state.lock().unwrap();
        if state.phase != BridgeState::Ready {
            return reply_response(503, "Bridge unavailable");
        }
        if state.active.is_some() {
            return reply_response(409, "Local inference is busy");
        }
        state.next_request += 1;
        let (id, abort) = (state.next_request, AbortSignal::new());
        state.active = Some(ActiveRequest { id, abort: abort.clone() });
        state.idle_generation += 1;
        (id, abort)
    };
    let (completed, destroyed) = (Arc::new(AtomicBool::new(false)), Arc::new(AtomicBool::new(false)));
    let (head_sender, head_receiver) = oneshot::channel();
    let mut guard = ClientGuard { abort: abort.clone(), completed: Arc::clone(&completed), finished: false };
    let exchange = Exchange { head: Some(head_sender), body: None, ended: false, abort, completed, destroyed: Arc::clone(&destroyed) };
    tokio::spawn(run_exchange(inner, id, Inbound { url, route, headers: parts.headers, body }, exchange));
    match head_receiver.await {
        Ok(Head::Full(response)) => {
            guard.finished = true;
            response
        }
        Ok(Head::Stream { status, content_type, receiver }) => {
            let mut response = Response::new(Body::from_stream(ClientBody { guard, receiver, destroyed, done: false }));
            *response.status_mut() = status;
            let headers = response.headers_mut();
            headers.insert(header::CONTENT_TYPE, content_type);
            headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
            response
        }
        Err(_) => {
            guard.finished = true;
            reply_response(502, "Local upstream failed")
        }
    }
}

async fn run_exchange(inner: Arc<BridgeInner>, id: u64, inbound: Inbound, mut exchange: Exchange) {
    let (abort, completed) = (exchange.abort.clone(), Arc::clone(&exchange.completed));
    let timeout = inner.options.request_timeout;
    let mut timed_out = false;
    let outcome = {
        let work = forward(&inner, &mut exchange, inbound);
        if timeout.is_zero() {
            work.await
        } else {
            tokio::select! {
                result = work => result,
                () = tokio::time::sleep(timeout) => {
                    timed_out = true;
                    Err(aborted())
                }
            }
        }
    };
    if timed_out {
        exchange.reply(504, "Local inference timed out");
        abort.abort();
    }
    if let Err(error) = outcome
        && !completed.load(Ordering::SeqCst)
    {
        match error.status {
            Some(status) => exchange.reply(status, &error.message),
            None => {
                exchange.reply(502, "Local upstream failed");
                if !abort.is_aborted() {
                    exchange.close();
                    stop_bridge(&inner, "Local upstream or usage failure").await;
                }
            }
        }
    }
    exchange.close();
    // 자원 소유자가 원격 취소를 확인해야 모델을 다시 쓴다. 확인 못 한 취소는 자원을 내린다.
    if abort.is_aborted() {
        stop_bridge(&inner, "Request cancelled").await;
    }
    {
        let mut state = inner.state.lock().unwrap();
        if state.active.as_ref().is_some_and(|active| active.id == id) {
            state.active = None;
        }
    }
    arm_idle(&inner);
}

pub(super) async fn read_request_body(body: Body, limit: usize, abort: &AbortSignal) -> Result<Vec<u8>, LocalError> {
    let cancelled = || LocalError::status(499, "Request cancelled");
    if abort.is_aborted() {
        return Err(cancelled());
    }
    let mut stream = body.into_data_stream();
    let mut bytes = Vec::new();
    loop {
        let chunk = tokio::select! {
            chunk = stream.next() => chunk,
            () = abort.aborted() => return Err(cancelled()),
        };
        match chunk {
            None => return Ok(bytes),
            Some(Err(_)) => return Err(LocalError::status(499, "Client disconnected")),
            Some(Ok(chunk)) => {
                if bytes.len() + chunk.len() > limit {
                    return Err(LocalError::status(413, "Body too large"));
                }
                bytes.extend_from_slice(&chunk);
            }
        }
    }
}

async fn next_chunk(response: &mut reqwest::Response, abort: &AbortSignal) -> Result<Option<Bytes>, LocalError> {
    tokio::select! {
        chunk = response.chunk() => chunk.map_err(|error| LocalError::plain(error.to_string())),
        () = abort.aborted() => Err(aborted()),
    }
}

/// pipeline(response, ...transforms, res). 끝에서 앞 변환의 마지막 출력이 다음 변환을 지난다.
fn through(transforms: &mut [Box<dyn StreamTransform>], chunk: &[u8]) -> Result<Vec<u8>, String> {
    let mut data = chunk.to_vec();
    for transform in transforms.iter_mut() {
        data = transform.push(&data)?;
    }
    Ok(data)
}

fn finish(transforms: &mut [Box<dyn StreamTransform>]) -> Result<Vec<u8>, String> {
    let mut data = Vec::new();
    for transform in transforms.iter_mut() {
        let mut output = if data.is_empty() { Vec::new() } else { transform.push(&data)? };
        output.extend(transform.finish()?);
        data = output;
    }
    Ok(data)
}

async fn forward(inner: &Arc<BridgeInner>, exchange: &mut Exchange, inbound: Inbound) -> Result<(), LocalError> {
    let options = &inner.options;
    let abort = exchange.abort.clone();
    let sample = inner.resources().await?;
    if sample.busy_flag() {
        return Err(LocalError::status(503, "GPU reserved by another workload"));
    }
    let body = read_request_body(inbound.body, options.max_body_bytes, &abort).await?;
    let parsed = parse_js_json(&String::from_utf8_lossy(&body)).map_err(|_| LocalError::status(400, "Invalid JSON"))?;
    if !parsed.is_object() {
        return Err(LocalError::status(400, "Invalid JSON object"));
    }
    if parsed.get("model").and_then(Value::as_str) != Some(options.model.as_str()) {
        return Err(LocalError::status(400, "Model does not match local profile"));
    }
    if let Some(max_tokens) = parsed.get("max_tokens")
        && !safe_nonnegative_integer(max_tokens).is_some_and(|tokens| tokens > 0 && tokens <= options.max_output_tokens)
    {
        return Err(LocalError::status(400, "Unverified output budget"));
    }
    if abort.is_aborted() {
        return Err(LocalError::status(503, "Request cancelled"));
    }
    let llama_responses = options.usage_profile == LLAMA_RESPONSES_PROFILE;
    let adapter = if options.usage_protocol == RESPONSES_USAGE_PROTOCOL {
        let request = normalize_local_responses_request(&parsed, &options.reasoning, &options.usage_profile, options.context_window, Some(&options.model))?;
        Some(adapt_local_responses_tools(&request, ToolOptions { image_tool_results_as_messages: llama_responses, reasoning_content_as_summary: llama_responses })?)
    } else {
        None
    };
    let owned = match &adapter {
        Some(_) => None,
        None if options.usage_profile == LLAMA_MESSAGES_PROFILE => Some(normalize_llama_messages_request(&parsed, &options.reasoning, Some(&options.model))?),
        None => {
            let message = normalize_local_message_request(&parsed)?;
            let applied = apply_local_reasoning(message.as_ref().unwrap_or(&parsed), &options.reasoning)?;
            applied.or(message)
        }
    };
    let normalized = adapter.as_ref().map(|adapter| &adapter.request).or(owned.as_ref());
    (options.observe_request)(normalized.unwrap_or(&parsed));
    let outgoing = match normalized {
        Some(value) => to_js_json(value).map_err(|error| LocalError::plain(error.to_string()))?,
        None => body,
    };
    let target = inner.upstream.join(&inbound.url).map_err(|error| LocalError::plain(error.to_string()))?;
    let mut request = inner.http.post(target).header(header::CONTENT_TYPE, "application/json").header(header::ACCEPT_ENCODING, "identity");
    for key in ["anthropic-version", "anthropic-beta", "accept"] {
        if let Some(value) = inbound.headers.get(key) {
            request = request.header(key, value.clone());
        }
    }
    let mut response = tokio::select! {
        response = request.body(outgoing).send() => response.map_err(|error| LocalError::plain(error.to_string()))?,
        () = abort.aborted() => return Err(aborted()),
    };
    let status = response.status().as_u16();
    let content_type = response.headers().get(header::CONTENT_TYPE).filter(|value| !value.is_empty()).cloned().unwrap_or(HeaderValue::from_static("application/json"));
    if status != 200 || inbound.route.ends_with("/count_tokens") {
        exchange.write_head(status, content_type);
        while let Some(chunk) = next_chunk(&mut response, &abort).await? {
            exchange.write(chunk).await?;
        }
        exchange.end();
        return Ok(());
    }
    if response.headers().get(header::CONTENT_ENCODING).is_some_and(|encoding| !encoding.is_empty() && encoding != "identity") {
        return Err(LocalError::status(502, "Unexpected upstream encoding"));
    }
    let on_completed = {
        let (completed, hook) = (Arc::clone(&exchange.completed), Arc::clone(&options.on_response_completed));
        move || {
            completed.store(true, Ordering::SeqCst);
            hook();
        }
    };
    let on_usage = {
        let audit = Arc::clone(&options.audit);
        move |record: UsageRecord| audit(record)
    };
    if String::from_utf8_lossy(content_type.as_bytes()).contains("text/event-stream") {
        exchange.write_head(200, content_type);
        let mut transforms: Vec<Box<dyn StreamTransform>> = match &adapter {
            Some(adapter) => vec![
                Box::new(adapter.stream()),
                Box::new(ResponsesUsageSseTransform::new(&options.usage_profile, Some(Box::new(on_usage)), Some(Box::new(on_completed))).map_err(LocalError::plain)?),
            ],
            None => vec![Box::new(UsageSseTransform::new(&options.usage_profile, Some(Box::new(on_usage)), Some(Box::new(on_completed))).map_err(LocalError::plain)?)],
        };
        while let Some(chunk) = next_chunk(&mut response, &abort).await? {
            let output = through(&mut transforms, &chunk).map_err(LocalError::plain)?;
            exchange.write(Bytes::from(output)).await?;
        }
        let output = finish(&mut transforms).map_err(LocalError::plain)?;
        exchange.write(Bytes::from(output)).await?;
        exchange.end();
        return Ok(());
    }
    let mut raw = Vec::new();
    while let Some(chunk) = next_chunk(&mut response, &abort).await? {
        if raw.len() + chunk.len() > options.max_body_bytes {
            return Err(LocalError::status(413, "Body too large"));
        }
        raw.extend_from_slice(&chunk);
    }
    let result = parse_js_json(&String::from_utf8_lossy(&raw)).map_err(|error| LocalError::plain(format!("SyntaxError: {error}")))?;
    let result = match &adapter {
        Some(adapter) => {
            if let Some(record) = normalize_responses_usage(&result, &options.usage_profile).map_err(LocalError::plain)? {
                on_usage(record);
            }
            adapter.restore_response(&result)?
        }
        None => {
            let mut normalizer = UsageNormalizer::new(&options.usage_profile, Some(Box::new(on_usage))).map_err(LocalError::plain)?;
            let next = normalizer.apply(&result).map_err(LocalError::plain)?.unwrap_or(result);
            if options.usage_profile == LLAMA_MESSAGES_PROFILE && next.get("type").and_then(Value::as_str) == Some("message") && js_truthy(next.get("stop_reason")) {
                on_completed();
            }
            next
        }
    };
    exchange.send_full(200, content_type, to_js_json(&result).map_err(|error| LocalError::plain(error.to_string()))?);
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicUsize;

    use serde_json::Map;
    use tokio::sync::Notify;

    use super::super::host::{RAM_GUARD_PERCENT, VRAM_GUARD_PERCENT};
    use super::super::usage::{INCLUSIVE_INPUT_PROFILE, RESPONSES_INCLUSIVE_PROFILE};
    use super::*;

    const TOKEN: &str = "isolated-bridge-test-token-32bytes";
    const MESSAGES: &str = "/v1/messages";
    const RESPONSES: &str = "/v1/responses";

    #[derive(Clone)]
    struct Seen {
        body: String,
        headers: HeaderMap,
        url: String,
    }

    type Reply = Arc<dyn Fn(&Seen) -> BoxFuture<'static, Response> + Send + Sync>;

    fn respond(status: u16, content_type: &str, body: Body) -> Response {
        let mut response = Response::new(body);
        *response.status_mut() = code(status);
        response.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_str(content_type).unwrap());
        response
    }

    fn json_reply(value: Value) -> Reply {
        Arc::new(move |_| {
            let body = value.to_string();
            Box::pin(async move { respond(200, "application/json", Body::from(body)) })
        })
    }

    fn message_reply() -> Reply {
        json_reply(json!({"type":"message","id":"m1","content":[{"type":"text","text":"hello"}],"usage":{"input_tokens":100,"cache_read_input_tokens":80,"output_tokens":2}}))
    }

    fn responses_reply() -> Reply {
        json_reply(json!({"id":"resp-test","object":"response","status":"completed","output":[],
            "usage":{"input_tokens":100,"output_tokens":12,"total_tokens":112,"input_tokens_details":{"cached_tokens":80},"output_tokens_details":{"reasoning_tokens":10}}}))
    }

    fn never() -> Reply {
        Arc::new(|_| Box::pin(std::future::pending()))
    }

    fn sse_reply(chunks: Vec<String>) -> Reply {
        Arc::new(move |_| {
            let chunks = chunks.clone();
            Box::pin(async move {
                let stream = futures_util::stream::iter(chunks.into_iter().map(|chunk| Ok::<_, io::Error>(Bytes::from(chunk))));
                respond(200, "text/event-stream", Body::from_stream(stream))
            })
        })
    }

    /// 상류 가짜. 받은 본문·머리·주소를 남긴다.
    async fn upstream(reply: Reply) -> (String, Arc<Mutex<Vec<Seen>>>) {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let url = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
        let record = Arc::clone(&seen);
        let router = Router::new().fallback(move |request: Request| {
            let (record, reply) = (Arc::clone(&record), Arc::clone(&reply));
            async move {
                let (parts, body) = request.into_parts();
                let bytes = axum::body::to_bytes(body, usize::MAX).await.unwrap();
                let entry = Seen { body: String::from_utf8_lossy(&bytes).into_owned(), headers: parts.headers, url: parts.uri.to_string() };
                record.lock().unwrap().push(entry.clone());
                reply(&entry).await
            }
        });
        tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        (url, seen)
    }

    /// 측정값 원천. fresh면 읽을 때마다 sampledAt을 지금으로 찍는다(Node `{...current, sampledAt: Date.now()}`).
    #[derive(Clone)]
    struct Telemetry(Arc<Mutex<(ResourceSample, bool)>>);

    impl Telemetry {
        fn new(vram: f64, ram: f64) -> Self {
            Self(Arc::new(Mutex::new((ResourceSample::of(vram, ram, false, None), true))))
        }

        fn set(&self, key: &str, value: Value) {
            self.0.lock().unwrap().0.0.insert(key.into(), value);
        }

        fn freeze(&self) {
            self.0.lock().unwrap().1 = false;
        }

        fn reader(&self) -> SampleReader {
            let this = self.clone();
            Arc::new(move || {
                let (mut sample, fresh) = this.0.lock().unwrap().clone();
                if fresh {
                    sample.0.insert("sampledAt".into(), json!(now_milliseconds() as i64));
                }
                Box::pin(async move { Ok(sample) })
            })
        }
    }

    fn fixed(sample: ResourceSample) -> SampleReader {
        Arc::new(move || {
            let sample = sample.clone();
            Box::pin(async move { Ok(sample) })
        })
    }

    fn counter(count: &Arc<AtomicUsize>) -> ReleaseHook {
        let count = Arc::clone(count);
        Arc::new(move |_| {
            count.fetch_add(1, Ordering::SeqCst);
            Box::pin(async { Ok(()) })
        })
    }

    fn options(upstream: &str, read: SampleReader) -> BridgeOptions {
        BridgeOptions::new(upstream, TOKEN, "test-model", INCLUSIVE_INPUT_PROFILE, read)
    }

    struct Setup {
        bridge: LocalInferenceBridge,
        url: String,
        seen: Arc<Mutex<Vec<Seen>>>,
        audit: Arc<Mutex<Vec<UsageRecord>>>,
        released: Arc<AtomicUsize>,
        telemetry: Telemetry,
        client: reqwest::Client,
    }

    async fn setup(reply: Reply, change: impl FnOnce(&mut BridgeOptions)) -> Setup {
        let (upstream, seen) = upstream(reply).await;
        let (audit, released, telemetry) = (Arc::new(Mutex::new(Vec::new())), Arc::new(AtomicUsize::new(0)), Telemetry::new(70.0, 55.0));
        let mut options = options(&upstream, telemetry.reader());
        let record = Arc::clone(&audit);
        options.audit = Arc::new(move |usage| record.lock().unwrap().push(usage));
        options.release = counter(&released);
        options.poll = Duration::from_millis(20);
        options.idle = Duration::from_millis(10_000);
        change(&mut options);
        let bridge = LocalInferenceBridge::new(options).unwrap();
        let url = bridge.start().await.unwrap();
        Setup { bridge, url, seen, audit, released, telemetry, client: crate::http_client_builder().no_proxy().build().unwrap() }
    }

    impl Setup {
        fn request(&self, body: String, headers: &[(&str, &str)], path: &str) -> reqwest::RequestBuilder {
            let mut map = HeaderMap::new();
            map.insert("x-api-key", HeaderValue::from_static(TOKEN));
            for (key, value) in headers {
                map.insert(axum::http::HeaderName::from_bytes(key.as_bytes()).unwrap(), HeaderValue::from_str(value).unwrap());
            }
            self.client.post(format!("{}{path}", self.url)).headers(map).body(body)
        }

        /// `{model:'test-model', ...body}`.
        fn body(value: Value) -> String {
            let mut merged = json!({ "model": "test-model" });
            for (key, field) in value.as_object().unwrap() {
                merged[key] = field.clone();
            }
            merged.to_string()
        }

        async fn post(&self, value: Value, headers: &[(&str, &str)], path: &str) -> reqwest::Response {
            self.request(Self::body(value), headers, path).send().await.unwrap()
        }

        fn seen(&self) -> Vec<Seen> {
            self.seen.lock().unwrap().clone()
        }

        fn sent(&self, index: usize) -> Value {
            serde_json::from_str(&self.seen()[index].body).unwrap()
        }

        fn released(&self) -> usize {
            self.released.load(Ordering::SeqCst)
        }

        fn state(&self) -> BridgeState {
            self.bridge.status().state
        }

        async fn health(&self, key: &str, value: &str) -> reqwest::Response {
            self.client.get(format!("{}/health", self.url)).header(key, value).send().await.unwrap()
        }
    }

    async fn body_json(response: reqwest::Response) -> Value {
        serde_json::from_slice(&response.bytes().await.unwrap()).unwrap()
    }

    async fn until(predicate: impl Fn() -> bool) {
        for _ in 0..100 {
            if predicate() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("condition timed out");
    }

    fn responses(profile: &str) -> impl FnOnce(&mut BridgeOptions) {
        let profile = profile.to_string();
        move |options| {
            options.usage_protocol = RESPONSES_USAGE_PROTOCOL.into();
            options.usage_profile = profile;
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn direct_claude_bridge_preserves_usage_and_routes_selected_effort() {
        let reply = json_reply(json!({"type":"message","stop_reason":"end_turn","content":[{"type":"text","text":"ok"}],"usage":{"input_tokens":4,"cache_read_input_tokens":53,"output_tokens":20}}));
        let s = setup(reply, |options| {
            options.usage_profile = LLAMA_MESSAGES_PROFILE.into();
            options.reasoning = "medium".into();
        })
        .await;
        let response = s.post(json!({"messages":[{"role":"user","content":"hi"}],"max_tokens":512}), &[], MESSAGES).await;
        assert_eq!(response.status(), 200);
        assert_eq!(body_json(response).await["usage"]["input_tokens"], 4);
        assert_eq!(s.sent(0)["chat_template_kwargs"], json!({"reasoning_effort":"medium"}));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn only_llama_bridge_exposes_reasoning_through_summary_lane() {
        let item = json!({"id":"r1","type":"reasoning","summary":[],"content":[{"type":"reasoning_text","text":"Model emitted text"}]});
        for profile in [LLAMA_RESPONSES_PROFILE, RESPONSES_INCLUSIVE_PROFILE] {
            let s = setup(json_reply(json!({ "output": [item.clone()] })), responses(profile)).await;
            let response = s.post(json!({"input":"hello"}), &[], RESPONSES).await;
            assert_eq!(response.status(), 200);
            let actual = body_json(response).await["output"][0].clone();
            assert_eq!(actual["content"], item["content"]);
            let summary = if profile == LLAMA_RESPONSES_PROFILE { json!([{"type":"summary_text","text":"Model emitted text"}]) } else { json!([]) };
            assert_eq!(actual["summary"], summary);
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn llama_bridge_sends_historical_image_results_as_vision_messages() {
        let s = setup(responses_reply(), |options| {
            responses(LLAMA_RESPONSES_PROFILE)(options);
            options.reasoning = "medium".into();
        })
        .await;
        let image = json!({"type":"input_image","image_url":"data:image/png;base64,AA=="});
        let input = json!([{"type":"function_call","name":"view_image","call_id":"old-image","arguments":"{}"},{"type":"function_call_output","call_id":"old-image","output":[image]},{"type":"message","role":"user","content":"continue"}]);
        let response = s.post(json!({ "input": input }), &[], RESPONSES).await;
        assert_eq!(response.status(), 200);
        response.bytes().await.unwrap();
        let sent = s.sent(0);
        assert!(sent["input"][1]["output"].is_string());
        assert_eq!(sent["input"][2]["content"][1], image);
        assert_eq!(sent["input"][3]["content"], "continue");
        assert_eq!(sent["reasoning"], json!({"effort":"medium"}));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn responses_route_preserves_inputs_tools_and_usage_without_old_cap() {
        let s = setup(responses_reply(), responses(RESPONSES_INCLUSIVE_PROFILE)).await;
        let input = json!([{"type":"message","role":"user","content":[{"type":"input_text","text":"hello"}]}]);
        let tools = json!([{"type":"function","name":"read_file","parameters":{"type":"object"}}]);
        let bearer = format!("Bearer {TOKEN}");
        let body = json!({"input":input,"tools":tools,"stream":false,"reasoning":{"effort":"default"},"service_tier":"priority"});
        let response = s.post(body, &[("authorization", &bearer)], RESPONSES).await;
        assert_eq!(response.status(), 200);
        assert_eq!(body_json(response).await["usage"]["input_tokens"], 100);
        let sent = s.sent(0);
        assert_eq!((&sent["input"], &sent["tools"]), (&input, &tools));
        for key in ["max_output_tokens", "reasoning", "service_tier"] {
            assert!(sent.get(key).is_none(), "{key}");
        }
        assert!(s.seen()[0].headers.get("authorization").is_none() && s.seen()[0].headers.get("x-api-key").is_none());
        let audit = s.audit.lock().unwrap()[0].normalized.clone();
        assert_eq!((audit["input_tokens"].as_u64(), audit["output_tokens"].as_u64()), (Some(20), Some(12)));
        assert_eq!(s.post(json!({}), &[], MESSAGES).await.status(), 404);
        assert_eq!(s.post(json!({}), &[], "/v1/responses?url=http://evil").await.status(), 404);
        assert_eq!(s.post(json!({"max_output_tokens":8192}), &[], RESPONSES).await.status(), 200);
        assert_eq!(s.post(json!({"model":"other"}), &[], RESPONSES).await.status(), 400);
        assert_eq!(s.post(json!({}), &[("x-api-key", "wrong")], RESPONSES).await.status(), 401);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn responses_output_budget_cannot_exceed_measured_context() {
        let s = setup(responses_reply(), |options| {
            responses(RESPONSES_INCLUSIVE_PROFILE)(options);
            options.context_window = Some(65536);
        })
        .await;
        assert_eq!(s.post(json!({"max_output_tokens":65536}), &[], RESPONSES).await.status(), 200);
        assert_eq!(s.post(json!({"max_output_tokens":65537}), &[], RESPONSES).await.status(), 400);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn responses_sse_forwards_function_calls_and_completed_usage() {
        let events = [
            json!({"type":"response.output_item.done","item":{"type":"function_call","call_id":"call1","name":"read_file","arguments":"{}"}}),
            json!({"type":"response.completed","response":{"id":"r1","status":"completed","usage":{"input_tokens":40,"output_tokens":4,"total_tokens":44}}}),
        ];
        let raw: String = events.iter().map(|event| format!("data: {event}\n\n")).collect();
        let s = setup(sse_reply(vec![raw.clone()]), responses(RESPONSES_INCLUSIVE_PROFILE)).await;
        let response = s.post(json!({"stream":true}), &[], RESPONSES).await;
        assert_eq!(String::from_utf8(response.bytes().await.unwrap().to_vec()).unwrap(), raw);
        assert_eq!(s.audit.lock().unwrap().len(), 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn responses_thinking_off_reaches_upstream() {
        let s = setup(responses_reply(), |options| {
            responses(RESPONSES_INCLUSIVE_PROFILE)(options);
            options.reasoning = "off".into();
        })
        .await;
        assert_eq!(s.post(json!({"reasoning":{"effort":"xhigh"}}), &[], RESPONSES).await.status(), 200);
        assert_eq!(s.sent(0)["reasoning"], json!({"effort":"none"}));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn authenticated_bridge_delivers_thinking_override() {
        let s = setup(message_reply(), |options| options.reasoning = "on".into()).await;
        assert_eq!(s.post(json!({}), &[], MESSAGES).await.status(), 200);
        assert_eq!(s.sent(0)["thinking"], json!({"type":"enabled","budget_tokens":1024}));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn message_and_count_tokens_paths_adapt_tool_references() {
        let s = setup(message_reply(), |_| {}).await;
        let body = json!({"tools":[{"name":"SendMessage","input_schema":{"type":"object"}}],"messages":[{"role":"user","content":[{"type":"tool_result","tool_use_id":"real-shape","content":[{"type":"tool_reference","tool_name":"SendMessage"}]}]}]});
        for path in ["/v1/messages?beta=true", "/v1/messages/count_tokens"] {
            let response = s.post(body.clone(), &[], path).await;
            assert_eq!(response.status(), 200);
            response.bytes().await.unwrap();
        }
        let seen = s.seen();
        assert_eq!(seen.len(), 2);
        assert_eq!(seen[0].url, "/v1/messages?beta=true");
        for entry in &seen {
            let length: usize = entry.headers["content-length"].to_str().unwrap().parse().unwrap();
            assert_eq!(length, entry.body.len());
            let value: Value = serde_json::from_str(&entry.body).unwrap();
            assert_eq!(value["messages"][0]["content"][0]["content"][0]["type"], "text");
            assert_eq!(value["tools"][0]["name"], "SendMessage");
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cleanup_timeout_is_not_proof_of_release() {
        let gate = Arc::new(Notify::new());
        let mut options = options("http://127.0.0.1:1234", Telemetry::new(1.0, 1.0).reader());
        let wait = Arc::clone(&gate);
        options.release = Arc::new(move |_| {
            let wait = Arc::clone(&wait);
            Box::pin(async move {
                wait.notified().await;
                Ok(())
            })
        });
        options.cleanup_timeout = Duration::from_millis(50);
        let bridge = LocalInferenceBridge::new(options).unwrap();
        bridge.stop("manual").await;
        assert_eq!((bridge.status().cleanup_complete, bridge.status().state), (false, BridgeState::Fault));
        gate.notify_one();
        until(|| bridge.status().cleanup_complete).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn auth_browser_origins_and_routes_are_blocked_before_upstream() {
        let s = setup(message_reply(), |_| {}).await;
        assert_eq!(s.client.get(format!("{}/health", s.url)).send().await.unwrap().status(), 401);
        assert_eq!(s.post(json!({}), &[("x-api-key", "wrong")], MESSAGES).await.status(), 401);
        assert_eq!(s.post(json!({}), &[("origin", "https://example.com")], MESSAGES).await.status(), 401);
        assert_eq!(s.post(json!({}), &[], "/v1/models").await.status(), 404);
        assert!(s.seen().is_empty());
        let health = body_json(s.health("authorization", &format!("Bearer {TOKEN}")).await).await;
        assert_eq!((health["state"].as_str(), health["usageProtocol"].as_str()), (Some("ready"), Some(ANTHROPIC_USAGE_PROTOCOL)));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn json_usage_normalized_raw_audited_request_exact_and_credentials_not_forwarded() {
        let s = setup(message_reply(), |_| {}).await;
        let body = r#" {"model":"test-model","messages":[{"role":"user","content":"한글"}]} "#;
        let bearer = format!("Bearer {TOKEN}");
        let headers = [("authorization", bearer.as_str()), ("cookie", "private=1"), ("x-secret", "hidden")];
        let result = body_json(s.request(body.into(), &headers, MESSAGES).send().await.unwrap()).await;
        assert_eq!((result["usage"]["input_tokens"].as_u64(), result["usage"]["cache_read_input_tokens"].as_u64()), (Some(20), Some(80)));
        assert_eq!(result["content"][0]["text"], "hello");
        assert_eq!(s.audit.lock().unwrap()[0].raw["input_tokens"], 100);
        assert_eq!(s.seen()[0].body, body);
        for key in ["authorization", "x-api-key", "cookie", "x-secret"] {
            assert!(s.seen()[0].headers.get(key).is_none(), "{key}");
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn sse_normalized_across_chunks_without_changing_text() {
        let event = json!({"type":"message_start","message":{"id":"m","usage":{"input_tokens":100,"cache_read_input_tokens":80}}}).to_string();
        let chunks = vec![format!("data: {}", &event[..20]), format!("{}\n\n", &event[20..]), r#"data: {"type":"content_block_delta","delta":{"text":"끝"}}"#.to_string() + "\n\n"];
        let s = setup(sse_reply(chunks), |_| {}).await;
        let text = String::from_utf8(s.post(json!({"stream":true}), &[], MESSAGES).await.bytes().await.unwrap().to_vec()).unwrap();
        assert!(text.contains(r#""input_tokens":20"#) && text.contains('끝'), "{text}");
        assert_eq!(s.audit.lock().unwrap().len(), 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn count_tokens_and_upstream_errors_are_not_usage_normalized() {
        let reply: Reply = Arc::new(|seen| {
            let count = seen.url.ends_with("count_tokens");
            Box::pin(async move {
                if count { respond(200, "application/json", Body::from(r#"{"input_tokens":100}"#)) } else { respond(429, "application/json", Body::from(r#"{"error":"busy"}"#)) }
            })
        });
        let s = setup(reply, |_| {}).await;
        assert_eq!(body_json(s.post(json!({}), &[], "/v1/messages/count_tokens").await).await, json!({"input_tokens":100}));
        assert_eq!(s.post(json!({}), &[], MESSAGES).await.status(), 429);
        assert!(s.audit.lock().unwrap().is_empty());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn one_request_at_a_time_shared_start_and_single_release() {
        let gate = Arc::new(Notify::new());
        let wait = Arc::clone(&gate);
        let reply: Reply = Arc::new(move |seen| {
            let (wait, inner) = (Arc::clone(&wait), message_reply());
            let seen = seen.clone();
            Box::pin(async move {
                wait.notified().await;
                inner(&seen).await
            })
        });
        let s = setup(reply, |_| {}).await;
        assert_eq!(s.bridge.start().await.unwrap(), s.url);
        let pending = tokio::spawn(s.request(Setup::body(json!({})), &[], MESSAGES).send());
        until(|| s.seen().len() == 1).await;
        assert_eq!(s.post(json!({}), &[], MESSAGES).await.status(), 409);
        gate.notify_one();
        pending.await.unwrap().unwrap().bytes().await.unwrap();
        tokio::join!(s.bridge.stop("manual"), s.bridge.stop("manual"));
        assert_eq!(s.released(), 1);
        assert!(s.bridge.start().await.is_err());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn busy_gpu_blocks_new_requests_without_stopping_other_workload() {
        let s = setup(message_reply(), |_| {}).await;
        s.telemetry.set("busy", json!(true));
        assert_eq!(s.post(json!({}), &[], MESSAGES).await.status(), 503);
        assert!(s.seen().is_empty());
        assert_eq!(s.state(), BridgeState::Ready);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn memory_guard_closes_listener_and_releases_resources() {
        let s = setup(message_reply(), |_| {}).await;
        s.telemetry.set("vramPct", json!(77));
        until(|| s.state() == BridgeState::Fault).await;
        assert_eq!(s.released(), 1);
    }

    /// vram·ram 한도를 따로 두고, 허용 범위 안의 요청은 통과한 뒤 넘은 쪽 한도에서 멈춘다.
    async fn budget_case(vram_guard: f64, ram_guard: f64, start: (f64, f64), exceed: (&str, f64)) {
        let s = setup(message_reply(), |options| {
            options.vram_guard_percent = vram_guard;
            options.ram_guard_percent = ram_guard;
        })
        .await;
        s.telemetry.set("vramPct", json!(start.0));
        s.telemetry.set("ramPct", json!(start.1));
        let first = s.post(json!({}), &[], MESSAGES).await;
        assert_eq!(first.status(), 200);
        first.bytes().await.unwrap();
        s.telemetry.set(exceed.0, json!(exceed.1));
        until(|| s.state() == BridgeState::Fault).await;
        assert_eq!(s.released(), 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn explicit_gpu_budget_accepts_above_77_then_aborts_at_95() {
        let s = setup(message_reply(), |options| {
            options.vram_guard_percent = 95.0;
            options.ram_guard_percent = 77.0;
        })
        .await;
        s.telemetry.set("vramPct", json!(94.9));
        assert_eq!(s.post(json!({}), &[], MESSAGES).await.status(), 200);
        let health = body_json(s.health("x-api-key", TOKEN).await).await;
        assert_eq!((health["vramGuardPercent"].clone(), health["ramGuardPercent"].clone()), (json!(95), json!(77)));
        drop(s);
        budget_case(95.0, 77.0, (94.9, 55.0), ("vramPct", 95.0)).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn raising_vram_budget_does_not_raise_ram_budget() {
        budget_case(95.0, 77.0, (90.0, 55.0), ("ramPct", 77.0)).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn gpu_tolerance_accepts_96_but_stops_at_98() {
        budget_case(98.0, 77.0, (96.12, 55.0), ("vramPct", 98.0)).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn production_budget_keeps_independent_ram_and_gpu_stops() {
        for exceed in [("ramPct", 90.0), ("vramPct", 98.0)] {
            let s = setup(message_reply(), |options| {
                options.vram_guard_percent = VRAM_GUARD_PERCENT;
                options.ram_guard_percent = RAM_GUARD_PERCENT;
            })
            .await;
            s.telemetry.set("vramPct", json!(96.05));
            s.telemetry.set("ramPct", json!(77.86));
            let first = s.post(json!({}), &[], MESSAGES).await;
            assert_eq!(first.status(), 200);
            first.bytes().await.unwrap();
            s.telemetry.set("ramPct", json!(89.99));
            let next = s.post(json!({}), &[], MESSAGES).await;
            assert_eq!(next.status(), 200);
            next.bytes().await.unwrap();
            assert_eq!(s.released(), 0);
            s.telemetry.set(exceed.0, json!(exceed.1));
            until(|| s.state() == BridgeState::Fault).await;
            assert_eq!(s.released(), 1);
        }
    }

    #[test]
    fn rejects_budgets_above_limits_and_non_numbers() {
        let base = || options("http://127.0.0.1:1234", Telemetry::new(70.0, 55.0).reader());
        for vram in [98.1, 100.0, f64::NAN] {
            let mut options = base();
            options.vram_guard_percent = vram;
            assert_eq!(LocalInferenceBridge::new(options).err().as_deref(), Some("Invalid port or memory guard"));
        }
        let mut options = base();
        options.ram_guard_percent = 96.0;
        assert!(LocalInferenceBridge::new(options).is_err());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn stale_or_missing_startup_sample_fails_closed() {
        let mut stale = ResourceSample::of(70.0, 55.0, false, None);
        stale.0.insert("sampledAt".into(), json!(now_milliseconds() as i64 - 10_000));
        for sample in [ResourceSample(Map::new()), stale, ResourceSample::of(70.0, 80.0, false, None)] {
            let releases = Arc::new(AtomicUsize::new(0));
            let mut options = options("http://127.0.0.1:1234", fixed(sample));
            options.release = counter(&releases);
            let bridge = LocalInferenceBridge::new(options).unwrap();
            assert!(bridge.start().await.is_err());
            assert_eq!((bridge.status().state, releases.load(Ordering::SeqCst)), (BridgeState::Fault, 1));
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn startup_cancellation_never_leaves_a_late_listener() {
        let (gate, called, releases) = (Arc::new(Notify::new()), Arc::new(AtomicBool::new(false)), Arc::new(AtomicUsize::new(0)));
        let (wait, mark) = (Arc::clone(&gate), Arc::clone(&called));
        let mut options = options(
            "http://127.0.0.1:1234",
            Arc::new(move || {
                mark.store(true, Ordering::SeqCst);
                let wait = Arc::clone(&wait);
                Box::pin(async move {
                    wait.notified().await;
                    Ok(ResourceSample::of(70.0, 55.0, false, None))
                })
            }),
        );
        options.release = counter(&releases);
        let bridge = LocalInferenceBridge::new(options).unwrap();
        let started = bridge.start();
        until(|| called.load(Ordering::SeqCst)).await;
        bridge.stop("manual").await;
        gate.notify_one();
        assert!(started.await.is_err());
        assert_eq!((bridge.status().address, releases.load(Ordering::SeqCst)), (None, 1));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn idle_shutdown_releases_model_without_another_request() {
        let s = setup(message_reply(), |options| options.idle = Duration::from_millis(40)).await;
        until(|| s.state() == BridgeState::Stopped).await;
        assert_eq!(s.released(), 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn timeout_cancels_upstream_and_clears_active_request() {
        let s = setup(never(), |options| options.request_timeout = Duration::from_millis(50)).await;
        let response = s.post(json!({}), &[], MESSAGES).await;
        assert_eq!(response.status(), 504);
        response.bytes().await.unwrap();
        until(|| !s.bridge.status().active).await;
        assert_eq!((s.released(), s.bridge.status().cleanup_complete), (1, true));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn production_local_inference_has_no_wall_clock_timeout() {
        assert_eq!(LOCAL_INFERENCE_REQUEST_TIMEOUT, Duration::ZERO);
        let inner = message_reply();
        let reply: Reply = Arc::new(move |seen| {
            let (inner, seen) = (Arc::clone(&inner), seen.clone());
            Box::pin(async move {
                tokio::time::sleep(Duration::from_millis(80)).await;
                inner(&seen).await
            })
        });
        let s = setup(reply, |_| {}).await;
        let response = s.post(json!({}), &[], MESSAGES).await;
        assert_eq!(response.status(), 200);
        response.bytes().await.unwrap();
        assert_eq!(s.state(), BridgeState::Ready);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn invalid_and_oversized_body_never_reaches_model() {
        let s = setup(message_reply(), |options| options.max_body_bytes = 32).await;
        assert_eq!(s.request("not json".into(), &[], MESSAGES).send().await.unwrap().status(), 400);
        assert_eq!(s.request("x".repeat(100), &[], MESSAGES).send().await.unwrap().status(), 413);
        assert!(s.seen().is_empty());
    }

    #[test]
    fn rejects_unknown_dialect_weak_token_and_remote_upstream() {
        let base = || options("http://127.0.0.1:1234", Telemetry::new(70.0, 55.0).reader());
        let cases: [(fn(&mut BridgeOptions), &str); 3] = [
            (|options| options.upstream = "https://example.com".into(), "Upstream must be a loopback URL"),
            (|options| options.token = "weak".into(), "Dedicated bridge token of at least 24 bytes required"),
            (|options| options.usage_profile = "unknown".into(), "Explicit verified local usage profile required"),
        ];
        for (change, message) in cases {
            let mut options = base();
            change(&mut options);
            assert_eq!(LocalInferenceBridge::new(options).err().as_deref(), Some(message));
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn sdk_beta_query_allowed_but_unknown_queries_and_model_changes_rejected() {
        let s = setup(message_reply(), |_| {}).await;
        assert_eq!(s.post(json!({}), &[], "/v1/messages?beta=true").await.status(), 200);
        assert_eq!(s.post(json!({}), &[], "/v1/messages?beta=false").await.status(), 404);
        assert_eq!(s.post(json!({"model":"another-model"}), &[], MESSAGES).await.status(), 400);
        assert_eq!(s.post(json!({"max_tokens":5000}), &[], MESSAGES).await.status(), 400);
        assert_eq!(s.seen().len(), 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn bad_usage_semantics_fail_closed_and_release() {
        let s = setup(json_reply(json!({"type":"message","usage":{"input_tokens":1,"cache_read_input_tokens":10}})), |_| {}).await;
        assert_eq!(s.post(json!({}), &[], MESSAGES).await.status(), 502);
        until(|| s.state() == BridgeState::Fault).await;
        assert_eq!(s.released(), 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn memory_guard_aborts_a_running_request() {
        let s = setup(never(), |_| {}).await;
        let pending = tokio::spawn(s.request(Setup::body(json!({})), &[], MESSAGES).send());
        until(|| s.seen().len() == 1).await;
        s.telemetry.set("ramPct", json!(78));
        until(|| s.state() == BridgeState::Fault).await;
        let _ = pending.await;
        until(|| !s.bridge.status().active).await;
        assert_eq!(s.released(), 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn loss_of_fresh_telemetry_while_ready_shuts_down() {
        let s = setup(message_reply(), |_| {}).await;
        s.telemetry.freeze();
        s.telemetry.set("sampledAt", json!(now_milliseconds() as i64 - 20_000));
        until(|| s.state() == BridgeState::Fault).await;
        assert_eq!(s.released(), 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn client_cancellation_releases_runtime_before_clearing_slot() {
        let s = setup(never(), |_| {}).await;
        let pending = tokio::spawn(s.request(r#"{"model":"test-model"}"#.into(), &[], MESSAGES).send());
        until(|| s.seen().len() == 1).await;
        pending.abort();
        until(|| !s.bridge.status().active).await;
        let status = s.bridge.status();
        assert_eq!((status.state, s.released(), status.cleanup_complete), (BridgeState::Fault, 1, true));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn port_conflict_cannot_close_the_existing_bridge() {
        let s = setup(message_reply(), |_| {}).await;
        let releases = Arc::new(AtomicUsize::new(0));
        let mut options = options("http://127.0.0.1:1234", Telemetry::new(70.0, 55.0).reader());
        options.port = url::Url::parse(&s.url).unwrap().port().unwrap();
        options.release = counter(&releases);
        let second = LocalInferenceBridge::new(options).unwrap();
        let error = second.start().await.unwrap_err();
        assert!(error.message.contains("EADDRINUSE"), "{}", error.message);
        assert_eq!(releases.load(Ordering::SeqCst), 1);
        assert_eq!(s.post(json!({}), &[], MESSAGES).await.status(), 200);
        assert_eq!(s.released(), 0);
    }
}
