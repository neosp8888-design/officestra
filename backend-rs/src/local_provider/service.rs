// 4090 로컬 제공자 서비스. Node local-provider-service.mjs의 LocalProviderService를 옮겼다.
// 앞문 주소는 모델보다 오래 살아 유휴 정리 뒤에도 CLI의 기본 주소가 그대로다. 끊긴 추론은 다시 보내지 않는다.

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex, MutexGuard, Weak};
use std::time::{Duration, Instant};

use axum::Router;
use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::response::Response;
use futures_util::future::{BoxFuture, FutureExt, Shared, join_all};
use regex::Regex;
use serde_json::{Value, json};
use sqlx::PgPool;
use tokio::sync::oneshot;

use super::bridge::{
    BridgeFactory, BridgeOptions, BridgeServer, BridgeState, ClientBody, ClientGuard, Exchange, Head, InferenceBridge, SampleReader,
    default_bridge_factory, read_request_body, safe_equal,
};
use super::host::{
    COMFY_RELEASE_MODES, HostTimings, LOCAL_HOST_RESOURCE_SAMPLE_TIMEOUT, ModelHost, PgHostLeases, RAM_GUARD_PERCENT, ResourceSample, SshShell,
    StartOptions, VRAM_GUARD_PERCENT, WindowsHost,
};
use super::llama::as_model_host;
use super::pool::{Borrowed, ModelPool, SharedResult, Unlock, local_model_pool_key, spawn_shared};
use super::usage::{INCLUSIVE_INPUT_PROFILE, LLAMA_MESSAGES_PROFILE, LLAMA_RESPONSES_PROFILE, RESPONSES_INCLUSIVE_PROFILE};
use super::{AbortSignal, LocalDefinition, LocalError, local_codex_model_catalog, normalize_local_definition};
use crate::agent_runtime::attachments::StagedAttachment;
use crate::agent_runtime::launch::{Environment, LaunchPaths, RuntimeCharacter};
use crate::agent_runtime::service::Broadcast;
use crate::js::{to_js_json, uv_error};
use crate::local_agent_profile::{LocalAgentLaunch, LocalLaunchInput, create_local_agent_launch};
use crate::local_model_capabilities::{DIRECT_RUNTIME, local_model_supports_vision};

/// LOCAL_PROVIDER_SAMPLE_TIMEOUT_MS. 원격 측정 한도보다 5초 길다.
pub const LOCAL_PROVIDER_SAMPLE_TIMEOUT: Duration = Duration::from_millis(LOCAL_HOST_RESOURCE_SAMPLE_TIMEOUT.as_millis() as u64 + 5000);
/// Node는 본문을 흘려 보내지만 hyper는 본문을 다 읽어야 끊김을 알린다. 그래서 먼저 읽고 상한을 둔다(다리는 8MB에서 413).
const FRONT_DOOR_BODY_LIMIT: usize = 64 * 1024 * 1024;
const PAUSED: &str = "로컬 모델이 중지되어 있습니다. 모델 실행 버튼을 눌러주세요.";
const CANCELLED: &str = "Local request cancelled";
const CLEANUP_UNCONFIRMED: &str = "Local resource cleanup is not confirmed";
const CLEANUP_FAILED: &str = "로컬 자원 정리 완료를 확인하지 못했습니다";
const ABORTED: &str = "The operation was aborted";
const CONTROL_BUSY: &str = "모델 실행 또는 중지 처리 중입니다.";

static IMAGE_PATH: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)\.(png|jpe?g|gif|webp|bmp|heic)$").unwrap());
static CODEX_MODELS_ROUTE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^/v1/models(?:\?client_version=[0-9A-Za-z._-]+)?$").unwrap());
static MESSAGES_ROUTE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^/v1/messages(?:/count_tokens)?(?:\?beta=true)?$").unwrap());

/// hostFactory. 시험은 가짜 호스트를 넣는다.
pub type HostFactory = Arc<dyn Fn(&LocalDefinition) -> Result<Arc<dyn ModelHost>, LocalError> + Send + Sync>;

/// Node 생성자 인자.
pub struct ServiceOptions {
    pub host_factory: HostFactory,
    pub bridge_factory: BridgeFactory,
    pub broadcast: Broadcast,
    pub wait: Duration,
    pub retry: Duration,
    pub idle: Duration,
    pub cleanup_wait: Duration,
    pub retain_models: bool,
}

impl ServiceOptions {
    /// 기본값. 호스트는 SSH로 4090을 다루고, LM Studio 프로필은 시작할 때 미확정 오류로 멈춘다(R01).
    pub fn new(pool: PgPool, state_directory: PathBuf, broadcast: Broadcast) -> Self {
        let host_factory: HostFactory = Arc::new(move |definition| {
            let host = WindowsHost::new(definition, Arc::new(SshShell), Arc::new(PgHostLeases(pool.clone())), state_directory.clone(), HostTimings::default())?;
            Ok(as_model_host(host))
        });
        Self {
            host_factory,
            bridge_factory: default_bridge_factory(),
            broadcast,
            wait: Duration::from_millis(300_000),
            retry: Duration::from_millis(2000),
            idle: Duration::from_millis(180_000),
            cleanup_wait: Duration::from_millis(15_000),
            retain_models: true,
        }
    }
}

type Done = Shared<BoxFuture<'static, ()>>;

/// 보내는 쪽이 사라지면 풀리는 공유 대기. Node의 settle·pending Promise 대신 쓴다.
fn completion() -> (oneshot::Sender<()>, Done) {
    let (sender, receiver) = oneshot::channel::<()>();
    (
        sender,
        async move {
            let _ = receiver.await;
        }
        .boxed()
        .shared(),
    )
}

struct Active {
    id: u64,
    abort: AbortSignal,
    completed: Arc<AtomicBool>,
    done: Done,
}

#[derive(Default)]
struct EntryState {
    controllers: HashMap<u64, AbortSignal>,
    users: u64,
    state: String,
    error: Option<String>,
    bridge: Option<Arc<dyn InferenceBridge>>,
    resource: Option<Arc<Borrowed>>,
    resources: Option<ResourceSample>,
    active: Option<Active>,
    cleanup_failed: bool,
    closed: bool,
    closing: Option<SharedResult>,
    pending: HashMap<u64, Done>,
    /// clearTimeout 대신 세대 번호가 바뀌면 유휴 닫기 타이머가 아무것도 하지 않는다.
    retire_generation: u64,
    server: Option<BridgeServer>,
}

/// 프로필·추론 조합마다 하나인 고정 앞문. 주소·토큰은 닫힐 때까지 그대로다.
struct Entry {
    key: String,
    definition: LocalDefinition,
    group: String,
    token: String,
    port: u16,
    state: Mutex<EntryState>,
}

impl Entry {
    fn lock(&self) -> MutexGuard<'_, EntryState> {
        self.state.lock().unwrap()
    }
}

#[derive(Default)]
struct ServiceState {
    entries: Vec<Arc<Entry>>,
    closed: bool,
    model_controls: HashMap<u64, Done>,
}

struct ServiceInner {
    options: ServiceOptions,
    models: ModelPool,
    http: reqwest::Client,
    state: Mutex<ServiceState>,
    next_id: AtomicU64,
}

#[derive(Clone)]
pub struct LocalProviderService(Arc<ServiceInner>);

impl LocalProviderService {
    pub fn new(options: ServiceOptions) -> Result<Self, String> {
        let http = crate::http_client_builder().no_proxy().build().map_err(|error| error.to_string())?;
        let models = ModelPool::new(options.idle, Duration::from_millis(3000), options.retain_models);
        Ok(Self(Arc::new(ServiceInner { options, models, http, state: Mutex::new(ServiceState::default()), next_id: AtomicU64::new(0) })))
    }

    /// status(). 열린 앞문을 연 순서대로 보여 준다.
    pub fn status(&self) -> Vec<Value> {
        self.0
            .entries()
            .iter()
            .map(|entry| {
                let state = entry.lock();
                json!({
                    "id": entry.key, "profileId": entry.definition.profile.id, "state": state.state, "error": state.error,
                    "users": state.users, "resources": state.resources,
                })
            })
            .collect()
    }

    /// modelStatus(definition). 실행·중지 처리 상태가 먼저고, 없으면 앞문 상태와 적재 여부로 정한다.
    pub fn model_status(&self, definition: &LocalDefinition) -> Result<Value, LocalError> {
        self.0.model_status(definition)
    }

    /// controlModel. 부른 쪽이 사라져도 끝까지 돌고, 종료가 기다린다.
    pub async fn control_model(&self, raw: &Value, action: &str, comfy_release: Option<&str>) -> Result<Value, LocalError> {
        let service = Arc::clone(&self.0);
        let id = service.next();
        let (finished, done) = completion();
        service.lock().model_controls.insert(id, done);
        let (sender, receiver) = oneshot::channel();
        let (raw, action, comfy_release) = (raw.clone(), action.to_string(), comfy_release.map(str::to_string));
        tokio::spawn(async move {
            let result = service.apply_model_control(&raw, &action, comfy_release).await;
            service.lock().model_controls.remove(&id);
            drop(finished);
            let _ = sender.send(result);
        });
        receiver.await.unwrap_or_else(|_| Err(LocalError::plain("Local model control stopped")))
    }

    /// closeIdleProfile. 쓰는 중이면 거절하고, 확인된 새 주소가 있으면 닫기 전에 넘긴다.
    pub async fn close_idle_profile(&self, profile_id: &str, verified_address: Option<&str>) -> Result<(), LocalError> {
        self.0.close_idle_profile(profile_id, verified_address.filter(|address| !address.is_empty())).await
    }

    pub async fn shutdown(&self) -> Result<(), LocalError> {
        self.0.shutdown().await
    }
}

impl ServiceInner {
    fn lock(&self) -> MutexGuard<'_, ServiceState> {
        self.state.lock().unwrap()
    }

    fn next(&self) -> u64 {
        self.next_id.fetch_add(1, Ordering::SeqCst) + 1
    }

    fn entries(&self) -> Vec<Arc<Entry>> {
        self.lock().entries.clone()
    }

    fn paused(&self, group: &str) -> bool {
        self.models.state().group(group).is_some_and(|group| group.paused)
    }

    /// change(e, state, error).
    fn change(&self, entry: &Entry, state: &str, error: Option<&str>) {
        {
            let mut current = entry.lock();
            current.state = state.to_string();
            current.error = error.map(str::to_string);
        }
        (self.options.broadcast)(json!({ "type": "local.changed", "profileId": entry.definition.profile.id, "state": state, "error": error }));
    }

    fn changed(&self, profile_id: &str) {
        (self.options.broadcast)(json!({ "type": "local.changed", "profileId": profile_id }));
    }

    fn model_status(&self, definition: &LocalDefinition) -> Result<Value, LocalError> {
        let key = local_model_pool_key(definition).map_err(LocalError::plain)?;
        let group = {
            let state = self.models.state();
            state.group(&key).map(|group| (group.loaded(), group.control_error.clone(), group.control_state, group.paused, group.locked))
        };
        let Some((loaded, control_error, control_state, paused, locked)) = group else {
            return Ok(json!({ "state": "idle", "error": null, "loaded": false, "users": 0, "active": false, "resources": null }));
        };
        let entries: Vec<(String, Option<String>, u64, Option<Value>)> = self
            .entries()
            .iter()
            .filter(|entry| entry.group == key)
            .map(|entry| {
                let state = entry.lock();
                (state.state.clone(), state.error.clone(), state.users, state.resources.as_ref().map(|sample| Value::Object(sample.0.clone())))
            })
            .collect();
        let error = control_error.or_else(|| entries.iter().find(|entry| entry.0 == "error").and_then(|entry| entry.1.clone()));
        let any = |name: &str| entries.iter().any(|entry| entry.0 == name);
        let state = control_state.unwrap_or(if error.as_deref().is_some_and(|error| !error.is_empty()) {
            "error"
        } else if paused {
            "stopped"
        } else if any("starting") {
            "starting"
        } else if any("waiting") {
            "waiting"
        } else if loaded {
            "ready"
        } else {
            "idle"
        });
        Ok(json!({
            "state": state, "error": error, "loaded": loaded, "users": entries.iter().map(|entry| entry.2).sum::<u64>(), "active": locked,
            "resources": entries.iter().find_map(|entry| entry.3.clone()),
        }))
    }
}

#[derive(Clone)]
struct Door {
    service: Weak<ServiceInner>,
    entry: Weak<Entry>,
}

/// 앞문 JSON 응답. Node 앞문 reply는 cache-control을 붙이지 않는다.
fn door_json(status: u16, value: &Value) -> Response {
    let mut response = Response::new(Body::from(to_js_json(value).unwrap_or_default()));
    *response.status_mut() = StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY);
    response.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static("application/json"));
    response
}

fn door_reply(status: u16, message: &str) -> Response {
    door_json(status, &json!({ "error": message }))
}

/// 루프백 앞문을 띄운다. 듣기는 동기로 열어 포트를 바로 안다.
fn spawn_entry(service: &Arc<ServiceInner>, key: String, definition: LocalDefinition, group: String) -> Result<Arc<Entry>, LocalError> {
    let listen = |error: io::Error| LocalError::plain(error.to_string());
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).map_err(listen)?;
    listener.set_nonblocking(true).map_err(listen)?;
    let port = listener.local_addr().map_err(listen)?.port();
    let listener = tokio::net::TcpListener::from_std(listener).map_err(listen)?;
    let mut bytes = [0_u8; 32];
    // SAFETY: 32바이트 버퍼를 운영체제 난수로 채운다.
    unsafe { libc::arc4random_buf(bytes.as_mut_ptr().cast(), bytes.len()) };
    let token = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    let entry = Arc::new(Entry { key, definition, group, token, port, state: Mutex::new(EntryState { state: "idle".into(), ..EntryState::default() }) });
    let door = Door { service: Arc::downgrade(service), entry: Arc::downgrade(&entry) };
    let server = BridgeServer::spawn(listener, Router::new().fallback(front_door).with_state(door));
    entry.lock().server = Some(server);
    Ok(entry)
}

async fn front_door(State(door): State<Door>, request: Request) -> Response {
    let (Some(service), Some(entry)) = (door.service.upgrade(), door.entry.upgrade()) else {
        return door_reply(503, "Local session is closed");
    };
    let (parts, body) = request.into_parts();
    let token = match parts.headers.get("x-api-key") {
        Some(value) => value.as_bytes().to_vec(),
        None => parts.headers.get(header::AUTHORIZATION).map_or_else(Vec::new, |value| {
            let bytes = value.as_bytes();
            bytes.strip_prefix(b"Bearer ").unwrap_or(bytes).to_vec()
        }),
    };
    if parts.headers.get(header::ORIGIN).is_some_and(|origin| !origin.is_empty()) || !safe_equal(&token, entry.token.as_bytes()) {
        return door_reply(401, "Unauthorized");
    }
    let url = parts.uri.path_and_query().map_or("/", |path| path.as_str()).to_string();
    let codex = entry.definition.profile.backend == "codex";
    if codex && parts.method == Method::GET && CODEX_MODELS_ROUTE.is_match(&url) {
        let mut response = door_json(200, &local_codex_model_catalog(&entry.definition.profile));
        response.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
        return response;
    }
    let supported = if codex { url == "/v1/responses" } else { MESSAGES_ROUTE.is_match(&url) };
    if parts.method != Method::POST || !supported {
        return door_reply(404, "Unsupported route");
    }
    let id = service.next();
    let abort = AbortSignal::new();
    let (completed, destroyed) = (Arc::new(AtomicBool::new(false)), Arc::new(AtomicBool::new(false)));
    let (finished, pending) = completion();
    {
        let mut state = entry.lock();
        state.controllers.insert(id, abort.clone());
        state.pending.insert(id, pending);
    }
    let (head_sender, head_receiver) = oneshot::channel();
    let mut guard = ClientGuard { abort: abort.clone(), completed: Arc::clone(&completed), finished: false };
    let exchange = Exchange { head: Some(head_sender), body: None, ended: false, abort, completed, destroyed: Arc::clone(&destroyed) };
    let headers = parts.headers;
    tokio::spawn(async move {
        handle(&service, &entry, id, &url, &headers, body, exchange).await;
        entry.lock().pending.remove(&id);
        drop(finished);
    });
    match head_receiver.await {
        Ok(Head::Full(mut response)) => {
            guard.finished = true;
            response.headers_mut().remove(header::CACHE_CONTROL);
            response
        }
        Ok(Head::Stream { status, content_type, receiver }) => {
            let mut response = Response::new(Body::from_stream(ClientBody { guard, receiver, destroyed, done: false }));
            *response.status_mut() = status;
            response.headers_mut().insert(header::CONTENT_TYPE, content_type);
            response
        }
        Err(_) => {
            guard.finished = true;
            door_reply(502, "Local upstream failed")
        }
    }
}

/// handle(e, req, res)의 요청 작업. 받는 쪽이 사라져도 잠금 해제·정리까지 끝낸다.
async fn handle(service: &Arc<ServiceInner>, entry: &Arc<Entry>, id: u64, url: &str, headers: &HeaderMap, body: Body, mut exchange: Exchange) {
    let (settle, done) = completion();
    let mut unlock = None;
    let mut activated = false;
    let outcome = serve(service, entry, id, url, headers, body, &mut exchange, &mut unlock, &mut activated, done).await;
    if let Err(error) = outcome {
        service.fail(entry, &mut exchange, activated, error).await;
    }
    if let Some(unlock) = unlock
        && unlock.release().await.is_err()
    {
        entry.lock().cleanup_failed = true;
        service.change(entry, "error", Some(CLEANUP_FAILED));
    }
    {
        let mut state = entry.lock();
        if activated && state.active.as_ref().is_some_and(|active| active.id == id) {
            state.active = None;
        }
        state.controllers.remove(&id);
    }
    exchange.close();
    drop(settle);
}

#[allow(clippy::too_many_arguments)]
async fn serve(
    service: &Arc<ServiceInner>,
    entry: &Arc<Entry>,
    id: u64,
    url: &str,
    headers: &HeaderMap,
    body: Body,
    exchange: &mut Exchange,
    unlock: &mut Option<Unlock>,
    activated: &mut bool,
    done: Done,
) -> Result<(), LocalError> {
    let abort = exchange.abort.clone();
    let body = read_request_body(body, FRONT_DOOR_BODY_LIMIT, &abort).await?;
    if service.paused(&entry.group) {
        return Err(LocalError::plain(PAUSED));
    }
    service.await_cleanup(entry, Some(&abort)).await?;
    let (waiting_service, waiting_entry) = (Arc::clone(service), Arc::clone(entry));
    *unlock = Some(
        service
            .models
            .acquire(&entry.group, &abort, move || {
                let idle = waiting_entry.lock().active.is_none();
                if idle {
                    waiting_service.change(&waiting_entry, "waiting", Some("같은 모델의 앞선 요청을 기다리는 중"));
                }
            })
            .await?,
    );
    service.await_cleanup(entry, Some(&abort)).await?;
    if service.paused(&entry.group) {
        return Err(LocalError::plain(PAUSED));
    }
    entry.lock().active = Some(Active { id, abort: abort.clone(), completed: Arc::clone(&exchange.completed), done });
    *activated = true;
    let address = service.ensure(entry, &abort).await?;
    service.change(entry, "ready", None);
    forward(service, entry, &address, url, headers, body, exchange).await
}

/// 다리로 보내고 응답을 그대로 흘린다. 상류 응답은 여기서 버려져 다리를 멈추기 전에 연결이 닫힌다.
async fn forward(service: &ServiceInner, entry: &Entry, address: &str, url: &str, headers: &HeaderMap, body: Vec<u8>, exchange: &mut Exchange) -> Result<(), LocalError> {
    let abort = exchange.abort.clone();
    let mut request = service.http.post(format!("{address}{url}")).header(header::CONTENT_TYPE, "application/json").header("x-api-key", entry.token.as_str());
    for key in ["anthropic-version", "anthropic-beta"] {
        if let Some(value) = headers.get(key).filter(|value| !value.is_empty()) {
            request = request.header(key, value.clone());
        }
    }
    let mut response = tokio::select! {
        response = request.body(body).send() => response.map_err(|error| LocalError::plain(error.to_string()))?,
        () = abort.aborted() => return Err(LocalError::plain(ABORTED)),
    };
    let status = response.status().as_u16();
    if status >= 500 {
        service.change(entry, "error", Some("로컬 연결 실패 — 다음 요청에서 다시 연결합니다"));
    }
    let content_type = response.headers().get(header::CONTENT_TYPE).cloned().unwrap_or(HeaderValue::from_static("application/json"));
    exchange.write_head(status, content_type);
    loop {
        let chunk = tokio::select! {
            chunk = response.chunk() => chunk.map_err(|error| LocalError::plain(error.to_string()))?,
            () = abort.aborted() => return Err(LocalError::plain(ABORTED)),
        };
        match chunk {
            Some(bytes) => exchange.write(bytes).await?,
            None => break,
        }
    }
    exchange.end();
    Ok(())
}

impl ServiceInner {
    /// handle()의 catch. 대기열에서 취소된 요청은 추론·다리를 갖지 않으므로 지금 모델을 쓰는 요청을 멈추지 않는다.
    async fn fail(&self, entry: &Entry, exchange: &mut Exchange, activated: bool, error: LocalError) {
        let abort = exchange.abort.clone();
        if exchange.completed.load(Ordering::SeqCst) {
            exchange.destroy();
            return;
        }
        if !activated {
            let (idle, bridge) = {
                let state = entry.lock();
                (state.active.is_none() && !state.closed, state.bridge.clone())
            };
            if idle && abort.is_aborted() {
                let ready = bridge.is_some_and(|bridge| bridge.status().state == BridgeState::Ready);
                self.change(entry, if ready { "ready" } else { "idle" }, None);
            }
            exchange.reply(if error.status == Some(413) { 413 } else { 503 }, &error.message);
            return;
        }
        let cancelled = abort.is_aborted();
        abort.abort();
        let bridge = entry.lock().bridge.clone();
        let cleanup_failed = match bridge {
            Some(bridge) => {
                bridge.stop(if cancelled { "Request cancelled" } else { "connection failed" }).await;
                !bridge.status().cleanup_complete
            }
            None => false,
        };
        entry.lock().cleanup_failed = cleanup_failed;
        if cancelled && !cleanup_failed {
            self.change(entry, "idle", None);
            exchange.reply(503, CANCELLED);
            return;
        }
        let message = if cleanup_failed { CLEANUP_FAILED.to_string() } else { format!("로컬 연결 실패: {}", error.message) };
        self.change(entry, "error", Some(&message));
        exchange.reply(503, &message);
    }

    /// awaitCleanup. 끝난 응답이 아직 흘러 나가는 중이면 정리를 기다리되, 동시 추론은 받지 않는다.
    async fn await_cleanup(&self, entry: &Entry, signal: Option<&AbortSignal>) -> Result<(), LocalError> {
        let previous = entry.lock().active.as_ref().map(|active| (active.completed.load(Ordering::SeqCst), active.abort.is_aborted(), active.done.clone()));
        if let Some((completed, aborted, done)) = previous
            && (completed || aborted)
        {
            if !completed {
                self.change(entry, "draining", Some("이전 로컬 요청을 정리하는 중"));
            }
            let cancelled = async {
                match signal {
                    Some(signal) => signal.aborted().await,
                    None => std::future::pending().await,
                }
            };
            tokio::select! {
                biased;
                () = done => {}
                () = cancelled => return Err(LocalError::plain(CANCELLED)),
                () = tokio::time::sleep(self.options.cleanup_wait) => return Err(LocalError::plain("Local cleanup is still pending")),
            }
        }
        let (closed, cleanup_failed, bridge) = {
            let state = entry.lock();
            (state.closed, state.cleanup_failed, state.bridge.clone())
        };
        if signal.is_some_and(AbortSignal::is_aborted) || closed {
            return Err(LocalError::plain(CANCELLED));
        }
        if cleanup_failed && !bridge.is_some_and(|bridge| bridge.status().cleanup_complete) {
            return Err(LocalError::plain(CLEANUP_UNCONFIRMED));
        }
        Ok(())
    }
}

impl ServiceInner {
    /// ensure. 준비된 다리가 쉬고 있으면 그대로 쓰고, 아니면 정리를 확인한 뒤 모델을 빌려 새 다리를 띄운다.
    async fn ensure(self: &Arc<Self>, entry: &Arc<Entry>, signal: &AbortSignal) -> Result<String, LocalError> {
        let (bridge, resource) = {
            let state = entry.lock();
            (state.bridge.clone(), state.resource.clone())
        };
        if let Some(bridge) = &bridge
            && bridge.status().state == BridgeState::Ready
            && let Some(resource) = &resource
        {
            let sample = resource.sample().await?;
            if !sample.busy() && resource.alive() {
                return Ok(bridge.status().address.unwrap_or_default());
            }
        }
        // 같은 호스트에 새 잠금을 잡기 전에 정리를 기다린다.
        if let Some(bridge) = bridge {
            bridge.stop("manual").await;
            if !bridge.status().cleanup_complete {
                return Err(LocalError::plain(CLEANUP_UNCONFIRMED));
            }
        }
        let deadline = Instant::now() + self.options.wait;
        while !signal.is_aborted() && !entry.lock().closed {
            let host = (self.options.host_factory)(&entry.definition)?;
            let mut borrowed = None;
            match self.start_bridge(entry, signal, host, &mut borrowed).await {
                Ok(address) => return Ok(address),
                Err(error) => {
                    if let Some(resource) = borrowed {
                        resource.release("Local startup failed").await?;
                    }
                    if !error.is_busy() || Instant::now() >= deadline {
                        return Err(error);
                    }
                    self.change(entry, "waiting", Some("ComfyUI 또는 다른 로컬 작업 종료를 기다리는 중"));
                    tokio::select! {
                        () = tokio::time::sleep(self.options.retry) => {}
                        () = signal.aborted() => return Err(LocalError::plain(ABORTED)),
                    }
                }
            }
        }
        Err(LocalError::plain(CANCELLED))
    }

    async fn start_bridge(self: &Arc<Self>, entry: &Arc<Entry>, signal: &AbortSignal, host: Arc<dyn ModelHost>, borrowed: &mut Option<Arc<Borrowed>>) -> Result<String, LocalError> {
        let (service, starting, start_signal) = (Arc::clone(self), Arc::clone(entry), signal.clone());
        let resource = self
            .models
            .borrow(&entry.group, move || {
                service.change(&starting, "starting", None);
                host.start(StartOptions { signal: start_signal, comfy_release: None })
            })
            .await?;
        let resource = Arc::new(resource);
        *borrowed = Some(Arc::clone(&resource));
        entry.lock().resource = Some(Arc::clone(&resource));
        let bridge = (self.options.bridge_factory)(self.bridge_options(entry, &resource)).map_err(LocalError::plain)?;
        entry.lock().bridge = Some(Arc::clone(&bridge));
        let address = bridge.start().await?;
        if signal.is_aborted() || entry.lock().closed {
            bridge.stop("manual").await;
            return Err(LocalError::plain("Local startup cancelled"));
        }
        entry.lock().cleanup_failed = false;
        self.change(entry, "ready", None);
        Ok(address)
    }

    fn bridge_options(self: &Arc<Self>, entry: &Arc<Entry>, resource: &Arc<Borrowed>) -> BridgeOptions {
        let profile = &entry.definition.profile;
        let codex = profile.backend == "codex";
        let usage_profile = match (profile.runtime.as_deref() == Some(DIRECT_RUNTIME), codex) {
            (true, true) => LLAMA_RESPONSES_PROFILE,
            (true, false) => LLAMA_MESSAGES_PROFILE,
            (false, true) => RESPONSES_INCLUSIVE_PROFILE,
            (false, false) => INCLUSIVE_INPUT_PROFILE,
        };
        let (reader, reading) = (Arc::clone(resource), Arc::downgrade(entry));
        let read_resources: SampleReader = Arc::new(move || {
            let (resource, entry) = (Arc::clone(&reader), reading.clone());
            Box::pin(async move {
                if !resource.alive() {
                    return Err(LocalError::plain("SSH tunnel disconnected"));
                }
                let sample = resource.sample().await?;
                if let Some(entry) = entry.upgrade() {
                    entry.lock().resources = Some(sample.clone());
                }
                Ok(sample)
            })
        });
        let mut options = BridgeOptions::new(resource.upstream(), entry.token.clone(), profile.model.clone(), usage_profile, read_resources);
        options.reasoning = profile.reasoning.clone().unwrap_or_else(|| "default".into());
        options.usage_protocol = profile.usage_protocol.clone();
        options.vram_guard_percent = VRAM_GUARD_PERCENT;
        options.ram_guard_percent = RAM_GUARD_PERCENT;
        options.context_window = Some(profile.context_window);
        options.max_output_tokens = profile.max_output_tokens;
        let completing = Arc::downgrade(entry);
        options.on_response_completed = Arc::new(move || {
            if let Some(entry) = completing.upgrade()
                && let Some(active) = &entry.lock().active
            {
                active.completed.store(true, Ordering::SeqCst);
            }
        });
        let (releasing, owner, service) = (Arc::clone(resource), Arc::downgrade(entry), Arc::downgrade(self));
        options.release = Arc::new(move |reason: String| {
            let (resource, entry, service) = (Arc::clone(&releasing), owner.clone(), service.clone());
            Box::pin(async move {
                resource.release(&reason).await?;
                if let (Some(entry), Some(service)) = (entry.upgrade(), service.upgrade()) {
                    // Node는 확인과 변경 사이에 다른 요청이 끼지 못한다. 여기서는 한 잠금 안에서 둘을 한다.
                    let changed = {
                        let mut state = entry.lock();
                        let idle = state.state != "error";
                        if idle {
                            state.state = "idle".into();
                            state.error = None;
                        }
                        idle
                    };
                    if changed {
                        (service.options.broadcast)(json!({ "type": "local.changed", "profileId": entry.definition.profile.id, "state": "idle", "error": null }));
                    }
                }
                Ok(())
            })
        });
        options.idle = self.options.idle;
        options.sample_timeout = LOCAL_PROVIDER_SAMPLE_TIMEOUT;
        options.sample_max_age = Duration::from_millis(15_000);
        options.poll = Duration::from_millis(3000);
        options
    }

    /// openEntry. 같은 프로필·추론은 앞문 하나를 같이 쓰고, 정의가 바뀌면 닫은 뒤에만 연다.
    async fn open_entry(self: &Arc<Self>, definition: LocalDefinition, terminal: bool) -> Result<(Arc<Entry>, EntryRelease), LocalError> {
        let group = self.models.group(&definition).map_err(LocalError::plain)?;
        if self.paused(&group) {
            return Err(LocalError::plain(PAUSED));
        }
        let key = format!("{}:{}", definition.profile.id, definition.profile.reasoning.as_deref().unwrap_or("default"));
        let closing = self.entries().into_iter().find(|entry| entry.key == key).and_then(|entry| entry.lock().closing.clone());
        if let Some(closing) = closing {
            closing.await?;
        }
        let entry = {
            let mut state = self.lock();
            let existing = state.entries.iter().find(|entry| entry.key == key).cloned();
            match existing {
                Some(entry) => entry,
                None => {
                    let entry = spawn_entry(self, key, definition.clone(), group)?;
                    state.entries.push(Arc::clone(&entry));
                    entry
                }
            }
        };
        if entry.definition != definition {
            return Err(LocalError::plain("Close existing local sessions before changing their profile"));
        }
        entry.lock().retire_generation += 1;
        self.await_cleanup(&entry, None).await?;
        {
            let mut state = entry.lock();
            if state.closed {
                return Err(LocalError::plain("Local session is closed"));
            }
            state.users += 1;
        }
        let release = EntryRelease(Arc::new(ReleaseInner {
            service: Arc::clone(self),
            entry: Arc::clone(&entry),
            terminal,
            catalog: Mutex::new(None),
            released: AtomicBool::new(false),
        }));
        Ok((entry, release))
    }

    /// close(e, {retireModel}). 서버·진행 요청·다리를 차례로 닫고, 같은 모델을 쓰는 앞문이 없을 때만 모델을 내린다.
    fn close(self: &Arc<Self>, entry: &Arc<Entry>, retire_model: bool) -> SharedResult {
        let mut state = entry.lock();
        if let Some(closing) = &state.closing {
            return closing.clone();
        }
        state.retire_generation += 1;
        state.closed = true;
        for controller in state.controllers.values() {
            controller.abort();
        }
        let server = state.server.take();
        let (service, entry) = (Arc::clone(self), Arc::clone(entry));
        let closing = spawn_shared(async move {
            if let Some(server) = server {
                server.close().await;
            }
            let pending: Vec<Done> = entry.lock().pending.values().cloned().collect();
            join_all(pending).await;
            let bridge = entry.lock().bridge.clone();
            if let Some(bridge) = bridge {
                bridge.stop("manual").await;
            }
            let shared = {
                let mut state = service.lock();
                state.entries.retain(|other| !Arc::ptr_eq(other, &entry));
                state.entries.iter().any(|other| other.group == entry.group)
            };
            let retain = service.models.state().group(&entry.group).is_some_and(|group| group.retain);
            if (retire_model || !retain) && !shared {
                service.models.retire(&entry.group).await?;
            }
            (service.options.broadcast)(json!({ "type": "local.changed", "profileId": entry.definition.profile.id, "state": "closed" }));
            Ok(())
        });
        state.closing = Some(closing.clone());
        closing
    }

    async fn close_idle_profile(self: &Arc<Self>, profile_id: &str, verified_address: Option<&str>) -> Result<(), LocalError> {
        let entries: Vec<Arc<Entry>> = self.entries().into_iter().filter(|entry| entry.definition.profile.id == profile_id).collect();
        let (groups, controlling, locked) = {
            let state = self.models.state();
            let groups: Vec<_> = state.groups.iter().filter(|group| group.profile_ids.iter().any(|id| id == profile_id)).collect();
            (
                groups.iter().map(|group| group.key.clone()).collect::<Vec<_>>(),
                groups.iter().any(|group| group.control_state.is_some()),
                groups.iter().any(|group| group.locked),
            )
        };
        if controlling {
            return Err(LocalError::busy(CONTROL_BUSY));
        }
        if entries.iter().any(|entry| {
            let state = entry.lock();
            state.users > 0 || state.active.is_some()
        }) {
            return Err(LocalError::busy("로컬 연결을 사용 중입니다. 작업을 마치고 터미널을 닫아주세요."));
        }
        if verified_address.is_some() && locked {
            return Err(LocalError::busy("같은 모델의 다른 요청이 끝난 뒤 주소를 변경하세요."));
        }
        for entry in &entries {
            let resource = entry.lock().resource.clone();
            if let (Some(address), Some(resource)) = (verified_address, resource) {
                resource.reconnect_address(address)?;
            }
            self.close(entry, true).await?;
        }
        for group in &groups {
            let resource = self.models.state().group(group).and_then(|group| group.record.as_ref()).map(|record| Arc::clone(&record.resource));
            if let (Some(address), Some(resource)) = (verified_address, resource) {
                resource.reconnect_address(address)?;
            }
            self.models.retire(group).await?;
        }
        Ok(())
    }
}

/// openEntry가 돌려주는 release. 한 번만 돌고, 부르지 않고 다 버려지면 대신 돌아 사용자 수가 남지 않는다.
#[derive(Clone)]
pub struct EntryRelease(Arc<ReleaseInner>);

struct ReleaseInner {
    service: Arc<ServiceInner>,
    entry: Arc<Entry>,
    terminal: bool,
    catalog: Mutex<Option<PathBuf>>,
    released: AtomicBool,
}

impl EntryRelease {
    pub async fn release(&self) -> Result<(), LocalError> {
        if self.0.released.swap(true, Ordering::SeqCst) {
            return Ok(());
        }
        release_entry(&self.0).await
    }

    fn keep_catalog(&self, directory: PathBuf) {
        *self.0.catalog.lock().unwrap() = Some(directory);
    }
}

impl Drop for ReleaseInner {
    fn drop(&mut self) {
        if self.released.swap(true, Ordering::SeqCst) {
            return;
        }
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let inner = ReleaseInner {
            service: Arc::clone(&self.service),
            entry: Arc::clone(&self.entry),
            terminal: self.terminal,
            catalog: Mutex::new(self.catalog.lock().unwrap().take()),
            released: AtomicBool::new(true),
        };
        runtime.spawn(async move {
            let _ = release_entry(&inner).await;
        });
    }
}

/// 임시 모델 목록을 지운다. OS가 먼저 지웠어도 GPU 임대가 묶이지 않게 실패는 경고만 남긴다.
async fn remove_catalog(directory: &Path) {
    let code = |error: &io::Error| uv_error(error).0;
    if let Err(error) = tokio::fs::remove_file(directory.join("models.json")).await
        && error.kind() != io::ErrorKind::NotFound
    {
        eprintln!("Local catalog cleanup failed: {}", code(&error));
    }
    if let Err(error) = tokio::fs::remove_dir(directory).await
        && error.kind() != io::ErrorKind::NotFound
    {
        eprintln!("Local catalog directory cleanup failed: {}", code(&error));
    }
}

async fn release_entry(inner: &ReleaseInner) -> Result<(), LocalError> {
    let catalog = inner.catalog.lock().unwrap().take();
    if let Some(directory) = catalog {
        remove_catalog(&directory).await;
    }
    let entry = &inner.entry;
    let remaining = {
        let mut state = entry.lock();
        state.users = state.users.saturating_sub(1);
        state.users
    };
    if remaining > 0 {
        return Ok(());
    }
    if inner.terminal {
        return inner.service.close(entry, false).await;
    }
    let generation = {
        let mut state = entry.lock();
        let keep = state.active.as_ref().filter(|active| active.completed.load(Ordering::SeqCst)).map(|active| active.id);
        for (id, controller) in &state.controllers {
            if Some(*id) != keep {
                controller.abort();
            }
        }
        state.retire_generation += 1;
        state.retire_generation
    };
    let (service, entry) = (Arc::clone(&inner.service), Arc::clone(entry));
    tokio::spawn(async move {
        tokio::time::sleep(service.options.idle).await;
        let current = entry.lock().retire_generation;
        if current != generation {
            return;
        }
        if let Err(error) = service.close(&entry, false).await {
            service.change(&entry, "error", Some(&error.message));
        }
    });
    Ok(())
}

/// launch()가 받는 실행 요청. Node launch({character, ...options})의 options다.
pub struct LaunchRequest<'a> {
    /// "gui" 또는 "terminal". 터미널은 release 때 앞문을 바로 닫는다.
    pub mode: &'a str,
    pub prompt: &'a str,
    pub attachments: &'a [StagedAttachment],
    pub previous_session_id: Option<&'a str>,
    pub workdir: &'a str,
    /// 없으면 process.env.
    pub base_environment: Option<&'a Environment>,
    pub hook_path: &'a str,
    pub executable: &'a str,
    pub paths: &'a LaunchPaths,
}

/// launch 결과. 실행이 끝나면 release를 부른다.
pub struct LocalLaunch {
    pub launch: LocalAgentLaunch,
    pub release: EntryRelease,
}

/// openMessagesDoor 결과. 롤플레이처럼 CLI 없이 Messages를 직접 보내는 호출자가 쓴다.
pub struct MessagesDoor {
    pub url: String,
    pub token: String,
    pub model: String,
    pub context_window: u64,
    pub max_output_tokens: u64,
    pub release: EntryRelease,
}

impl LocalProviderService {
    /// launch. 앞문을 열고 그 주소·토큰으로 CLI 실행 명세를 만든다. 실패하면 앞문 사용을 돌려준다.
    pub async fn launch(&self, character: &RuntimeCharacter, definition: &LocalDefinition, request: LaunchRequest<'_>) -> Result<LocalLaunch, LocalError> {
        let service = &self.0;
        if service.lock().closed {
            return Err(LocalError::plain("Local service is shutting down"));
        }
        if !local_model_supports_vision(&definition.profile) && request.attachments.iter().any(|attachment| attachment.is_codex_image || IMAGE_PATH.is_match(&attachment.path)) {
            return Err(LocalError::plain("31B는 텍스트 전용입니다. 이미지 분석은 G4 MeroMero 26B A4B · 비전 모델을 선택하세요."));
        }
        let (entry, release) = service.open_entry(definition.clone(), request.mode == "terminal").await?;
        match launch_spec(&entry, &release, character, request) {
            Ok(launch) => Ok(LocalLaunch { launch, release }),
            Err(error) => {
                release.release().await?;
                Err(error)
            }
        }
    }

    /// openMessagesDoor. 직원 CLI와 같은 앞문·모델 잠금을 써서 4090 모델을 함부로 바꾸지 않는다.
    pub async fn open_messages_door(&self, raw: &Value) -> Result<MessagesDoor, LocalError> {
        let service = &self.0;
        if service.lock().closed {
            return Err(LocalError::plain("Local service is shutting down"));
        }
        let definition = normalize_local_definition(raw).map_err(LocalError::plain)?;
        if definition.profile.backend != "claude" {
            return Err(LocalError::plain("Messages 형식을 쓰는 로컬 프로필만 연결할 수 있습니다."));
        }
        let (entry, release) = service.open_entry(definition.clone(), false).await?;
        Ok(MessagesDoor {
            url: format!("http://127.0.0.1:{}/v1/messages", entry.port),
            token: entry.token.clone(),
            model: definition.profile.model,
            context_window: definition.profile.context_window,
            max_output_tokens: definition.profile.max_output_tokens,
            release,
        })
    }
}

/// 공유 Codex 홈의 최신 클라우드 models_cache.json이 제공자 갱신을 막을 수 있어, 실행마다 모델 목록을 따로 고정한다.
fn launch_spec(entry: &Entry, release: &EntryRelease, character: &RuntimeCharacter, request: LaunchRequest<'_>) -> Result<LocalAgentLaunch, LocalError> {
    let mut profile = entry.definition.profile.clone();
    profile.endpoint = format!("http://127.0.0.1:{}", entry.port);
    let mut catalog_path = None;
    if profile.backend == "codex" {
        let directory = crate::usage_summary::make_temporary_directory("officestra-local-catalog-").map_err(LocalError::plain)?;
        release.keep_catalog(directory.clone());
        let path = directory.join("models.json");
        let bytes = to_js_json(&local_codex_model_catalog(&profile)).map_err(|error| LocalError::plain(error.to_string()))?;
        write_private(&path, &bytes).map_err(|error| LocalError::plain(error.to_string()))?;
        catalog_path = Some(path.to_string_lossy().into_owned());
    }
    let mut environment = request.base_environment.cloned().unwrap_or_else(crate::local_embedding::process_environment);
    environment.insert(profile.credential_env.clone(), entry.token.clone());
    create_local_agent_launch(LocalLaunchInput {
        profile: &profile,
        character,
        mode: request.mode,
        prompt: request.prompt,
        attachments: request.attachments,
        previous_session_id: request.previous_session_id,
        workdir: request.workdir,
        base_environment: &environment,
        hook_path: request.hook_path,
        catalog_path: catalog_path.as_deref(),
        executable: request.executable,
        paths: request.paths,
    })
    .map_err(LocalError::plain)
}

/// writeFile(path, data, {mode: 0o600}).
fn write_private(path: &Path, bytes: &[u8]) -> io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(path)?.write_all(bytes)
}

impl ServiceInner {
    async fn apply_model_control(self: &Arc<Self>, raw: &Value, action: &str, comfy_release: Option<String>) -> Result<Value, LocalError> {
        if self.lock().closed {
            return Err(LocalError::plain("Local service is shutting down"));
        }
        // unload는 롤플레이처럼 실행 버튼 없이 요청이 올 때 올리는 모델용이다. 잠그지 않고 내려서 다음 요청이 다시 올린다.
        if !["start", "stop", "unload"].contains(&action) {
            return Err(LocalError::plain("Unsupported model action"));
        }
        if let Some(mode) = &comfy_release
            && (action != "start" || !COMFY_RELEASE_MODES.contains(&mode.as_str()))
        {
            return Err(LocalError::plain("Unsupported ComfyUI release mode"));
        }
        let definition = normalize_local_definition(raw).map_err(LocalError::plain)?;
        let group = self.models.group(&definition).map_err(LocalError::plain)?;
        let controller = AbortSignal::new();
        {
            let mut state = self.models.state();
            let Some(record) = state.group_mut(&group) else {
                return Err(LocalError::plain("Unknown local model group"));
            };
            if record.control_state.is_some() {
                return Err(LocalError::busy(CONTROL_BUSY));
            }
            record.control_state = Some(if action == "start" { "starting" } else { "stopping" });
            record.control_error = None;
            record.control_abort = (action == "start").then(|| controller.clone());
            // 실행 버튼이 성공하기 전까지 기존·새 CLI 앞문을 모두 막는다. 중지해도 주소와 대화 정체는 그대로다.
            if action != "unload" {
                record.paused = true;
            }
        }
        self.changed(&definition.profile.id);
        let mut unlock = None;
        let result = self.run_model_control(&definition, &group, action, comfy_release, &controller, &mut unlock).await;
        if let Err(error) = &result
            && let Some(record) = self.models.state().group_mut(&group)
        {
            record.control_error = Some(error.message.clone());
        }
        let unlocked = match unlock {
            Some(unlock) => unlock.release().await,
            None => Ok(()),
        };
        if let Some(record) = self.models.state().group_mut(&group) {
            record.control_state = None;
            record.control_abort = None;
        }
        self.changed(&definition.profile.id);
        // Node finally의 unlock 실패가 본문 오류를 덮는다.
        unlocked?;
        result?;
        self.model_status(&definition)
    }

    async fn run_model_control(
        self: &Arc<Self>,
        definition: &LocalDefinition,
        group: &str,
        action: &str,
        comfy_release: Option<String>,
        controller: &AbortSignal,
        unlock: &mut Option<Unlock>,
    ) -> Result<(), LocalError> {
        if action != "start" {
            let entries: Vec<Arc<Entry>> = self.entries().into_iter().filter(|entry| entry.group == group).collect();
            for entry in &entries {
                for controller in entry.lock().controllers.values() {
                    controller.abort();
                }
            }
            let pending: Vec<Done> = entries.iter().flat_map(|entry| entry.lock().pending.values().cloned().collect::<Vec<_>>()).collect();
            join_all(pending).await;
            *unlock = Some(self.models.acquire(group, &AbortSignal::new(), || {}).await?);
            for entry in &entries {
                let bridge = entry.lock().bridge.clone();
                if let Some(bridge) = bridge {
                    bridge.stop("manual").await;
                }
                reset_entry(entry);
                self.change(entry, "idle", None);
            }
            return self.models.dispose_current(group).await;
        }
        *unlock = Some(self.models.acquire(group, controller, || {}).await?);
        let host = (self.options.host_factory)(definition)?;
        let signal = controller.clone();
        let resource = self.models.borrow(group, move || host.start(StartOptions { signal, comfy_release })).await?;
        resource.release("manual").await?;
        self.models.keep_warm_current(group);
        if let Some(record) = self.models.state().group_mut(group) {
            record.paused = false;
        }
        for entry in self.entries().into_iter().filter(|entry| entry.group == group) {
            // borrow()가 무효 기록을 바꾸기 전에 정리를 확인했다. 실패한 옛 다리가 되살린 모델을 계속 거절하면 안 된다.
            let (resource, bridge) = {
                let state = entry.lock();
                (state.resource.clone(), state.bridge.clone())
            };
            if !resource.is_some_and(|resource| resource.alive()) {
                if let Some(bridge) = bridge {
                    bridge.stop("manual").await;
                }
                reset_entry(&entry);
            }
            self.change(&entry, "idle", None);
        }
        Ok(())
    }

    async fn shutdown(self: &Arc<Self>) -> Result<(), LocalError> {
        self.lock().closed = true;
        for group in &self.models.state().groups {
            if let Some(abort) = &group.control_abort {
                abort.abort();
            }
        }
        let controls: Vec<Done> = self.lock().model_controls.values().cloned().collect();
        join_all(controls).await;
        let closings: Vec<SharedResult> = self.entries().iter().map(|entry| self.close(entry, false)).collect();
        for result in join_all(closings).await {
            result?;
        }
        self.models.shutdown().await
    }
}

/// `e.bridge=null;e.resource=null;e.cleanupFailed=false`.
fn reset_entry(entry: &Entry) {
    let mut state = entry.lock();
    state.bridge = None;
    state.resource = None;
    state.cleanup_failed = false;
}

#[cfg(test)]
pub(crate) mod tests {
    use std::future::Future;
    use std::sync::atomic::{AtomicI64, AtomicUsize};

    use axum::body::Bytes;
    use tokio::sync::watch;
    use tokio::task::JoinHandle;

    use super::super::host::HostResource;
    use super::super::local_resume_compatible;
    use super::super::pool::tests::definition_value as shared_definition_value;
    use super::super::tests::mero_definition;
    use super::*;
    use crate::local_model_capabilities::{MEROMERO_HOST_KEY, MEROMERO_MODEL_ID};

    fn ms(value: u64) -> Duration {
        Duration::from_millis(value)
    }

    async fn delay(value: u64) {
        tokio::time::sleep(ms(value)).await;
    }

    /// deferred(). 열면 기다리던 쪽이 모두 풀리고, 연 뒤에 기다리면 바로 풀린다.
    #[derive(Clone)]
    struct Gate(Arc<watch::Sender<bool>>);

    impl Gate {
        fn new() -> Self {
            Self(Arc::new(watch::channel(false).0))
        }

        fn open(&self) {
            self.0.send_replace(true);
        }

        async fn wait(&self) {
            let mut receiver = self.0.subscribe();
            let _ = receiver.wait_for(|open| *open).await;
        }
    }

    type Hook = Arc<dyn Fn() -> BoxFuture<'static, Result<(), LocalError>> + Send + Sync>;
    /// onRequest(n). false면 응답하지 않고 연결이 끊길 때까지 매달린다.
    type OnRequest = Arc<dyn Fn(usize) -> BoxFuture<'static, bool> + Send + Sync>;
    type Reply = Arc<dyn Fn(&str, &Value) -> Value + Send + Sync>;

    fn hook<F: Future<Output = Result<(), LocalError>> + Send + 'static>(run: impl Fn() -> F + Send + Sync + 'static) -> Hook {
        Arc::new(move || run().boxed())
    }

    fn on_request<F: Future<Output = bool> + Send + 'static>(run: impl Fn(usize) -> F + Send + Sync + 'static) -> OnRequest {
        Arc::new(move |count| run(count).boxed())
    }

    /// n번째 요청에서 reached를 열고 매달린다. 나머지는 바로 응답한다.
    fn hang_at(target: usize, reached: &Gate) -> OnRequest {
        let reached = reached.clone();
        on_request(move |count| {
            let reached = reached.clone();
            async move {
                if count == target {
                    reached.open();
                    return false;
                }
                true
            }
        })
    }

    /// local-provider-service 시험의 상류 응답.
    fn message_reply(_url: &str, _body: &Value) -> Value {
        json!({ "type": "message", "content": [{ "type": "text", "text": "ok" }], "usage": { "input_tokens": 100, "cache_read_input_tokens": 20, "output_tokens": 2 } })
    }

    /// local-model-pool 시험의 상류 응답. 보낸 표식을 그대로 돌려준다.
    fn echo_reply(url: &str, body: &Value) -> Value {
        if url == "/v1/responses" {
            json!({ "object": "response", "id": "r", "status": "completed",
                "output": [{ "type": "message", "role": "assistant", "content": [{ "type": "output_text", "text": body["input"] }] }],
                "usage": { "input_tokens": 12, "input_tokens_details": { "cached_tokens": 0 }, "output_tokens": 2, "total_tokens": 14 } })
        } else {
            json!({ "type": "message", "id": "m", "stop_reason": "end_turn", "content": [{ "type": "text", "text": body["messages"][0]["content"] }],
                "usage": { "input_tokens": 12, "cache_read_input_tokens": 0, "output_tokens": 2 } })
        }
    }

    /// 가짜 4090 상류. 받은 요청을 기록하고, 동시에 몇 개가 열렸는지 센다.
    struct Upstream {
        requests: Mutex<Vec<(String, Value)>>,
        on_request: Option<OnRequest>,
        fail: AtomicBool,
        reply: Reply,
        inflight: AtomicUsize,
        max_inflight: AtomicUsize,
    }

    /// res.once('close', ()=>inflight--). 응답을 돌려주거나 연결이 끊겨 처리기가 사라질 때 줄인다.
    struct Inflight(Arc<Upstream>);

    impl Drop for Inflight {
        fn drop(&mut self) {
            self.0.inflight.fetch_sub(1, Ordering::SeqCst);
        }
    }

    async fn upstream_handler(State(upstream): State<Arc<Upstream>>, request: Request) -> Response {
        let url = request.uri().path_and_query().map_or("/", |path| path.as_str()).to_string();
        let bytes = axum::body::to_bytes(request.into_body(), usize::MAX).await.unwrap_or_default();
        let body: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        let count = {
            let mut requests = upstream.requests.lock().unwrap();
            requests.push((url.clone(), body.clone()));
            requests.len()
        };
        let now = upstream.inflight.fetch_add(1, Ordering::SeqCst) + 1;
        upstream.max_inflight.fetch_max(now, Ordering::SeqCst);
        let _inflight = Inflight(Arc::clone(&upstream));
        if let Some(on_request) = &upstream.on_request
            && !on_request(count).await
        {
            std::future::pending::<()>().await;
        }
        if upstream.fail.swap(false, Ordering::SeqCst) {
            // res.destroy(). 본문이 오류로 끝나 연결이 끊긴다.
            let broken = futures_util::stream::once(async { Err::<Bytes, io::Error>(io::Error::other("destroyed")) });
            return Response::new(Body::from_stream(broken));
        }
        let mut response = Response::new(Body::from(serde_json::to_vec(&(upstream.reply)(&url, &body)).unwrap()));
        response.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static("application/json"));
        response
    }

    /// 가짜 호스트와 그 자원. 시작·해제 횟수와 주소 변경 요청을 센다.
    #[derive(Default)]
    struct Fake {
        upstream: String,
        vram: f64,
        ram: f64,
        busy: AtomicI64,
        starts: AtomicUsize,
        releases: AtomicUsize,
        on_release: Option<Hook>,
        wait_for_idle: Option<Hook>,
        reconnects: Mutex<Vec<String>>,
    }

    impl ModelHost for Fake {
        fn start(self: Arc<Self>, _options: StartOptions) -> BoxFuture<'static, Result<Arc<dyn HostResource>, LocalError>> {
            Box::pin(async move {
                self.starts.fetch_add(1, Ordering::SeqCst);
                if self.busy.fetch_sub(1, Ordering::SeqCst) > 0 {
                    return Err(LocalError::busy("busy"));
                }
                Ok(Arc::new(FakeResource(self)) as Arc<dyn HostResource>)
            })
        }
    }

    struct FakeResource(Arc<Fake>);

    impl HostResource for FakeResource {
        fn upstream(&self) -> String {
            self.0.upstream.clone()
        }

        fn sample(&self) -> BoxFuture<'static, Result<ResourceSample, LocalError>> {
            let sample = ResourceSample::of(self.0.vram, self.0.ram, false, None);
            Box::pin(async move { Ok(sample) })
        }

        fn alive(&self) -> bool {
            true
        }

        fn release(&self) -> BoxFuture<'static, Result<(), LocalError>> {
            self.0.releases.fetch_add(1, Ordering::SeqCst);
            let hook = self.0.on_release.clone();
            Box::pin(async move {
                match hook {
                    Some(hook) => hook().await,
                    None => Ok(()),
                }
            })
        }

        fn supports_wait_for_idle(&self) -> bool {
            self.0.wait_for_idle.is_some()
        }

        fn wait_for_idle(&self) -> BoxFuture<'static, Result<(), LocalError>> {
            let hook = self.0.wait_for_idle.clone();
            Box::pin(async move {
                match hook {
                    Some(hook) => hook().await,
                    None => Ok(()),
                }
            })
        }

        fn reconnect_address(&self, address: &str) -> Result<(), LocalError> {
            self.0.reconnects.lock().unwrap().push(address.to_string());
            Ok(())
        }
    }

    /// setup()·fixture() 인자. 기본값은 local-provider-service 시험의 setup()이다.
    pub(crate) struct Options {
        busy: i64,
        fail: bool,
        idle_ms: u64,
        vram: f64,
        ram: f64,
        on_request: Option<OnRequest>,
        on_release: Option<Hook>,
        wait_for_idle: Option<Hook>,
        cleanup_wait_ms: u64,
        retain: bool,
        reply: Reply,
    }

    impl Default for Options {
        fn default() -> Self {
            Self {
                busy: 0,
                fail: false,
                idle_ms: 10_000,
                vram: 70.0,
                ram: 55.0,
                on_request: None,
                on_release: None,
                wait_for_idle: None,
                cleanup_wait_ms: 15_000,
                retain: true,
                reply: Arc::new(message_reply),
            }
        }
    }

    impl Options {
        /// local-model-pool 시험의 fixture() 기본값.
        fn pool() -> Self {
            Self { vram: 95.0, ram: 50.0, retain: false, reply: Arc::new(echo_reply), ..Self::default() }
        }
    }

    /// 가짜 4090 호스트를 붙인 서비스. 터미널 관리자 시험도 쓴다.
    pub(crate) struct Harness {
        pub(crate) service: LocalProviderService,
        fake: Arc<Fake>,
        upstream: Arc<Upstream>,
        events: Arc<Mutex<Vec<Value>>>,
        http: reqwest::Client,
        releases: Mutex<Vec<EntryRelease>>,
        _server: BridgeServer,
    }

    pub(crate) fn harness(options: Options) -> Harness {
        let upstream = Arc::new(Upstream {
            requests: Mutex::new(Vec::new()),
            on_request: options.on_request,
            fail: AtomicBool::new(options.fail),
            reply: options.reply,
            inflight: AtomicUsize::new(0),
            max_inflight: AtomicUsize::new(0),
        });
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        listener.set_nonblocking(true).unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = BridgeServer::spawn(tokio::net::TcpListener::from_std(listener).unwrap(), Router::new().fallback(upstream_handler).with_state(Arc::clone(&upstream)));
        let fake = Arc::new(Fake {
            upstream: format!("http://127.0.0.1:{port}"),
            vram: options.vram,
            ram: options.ram,
            busy: AtomicI64::new(options.busy),
            on_release: options.on_release,
            wait_for_idle: options.wait_for_idle,
            ..Fake::default()
        });
        let host = Arc::clone(&fake);
        let host_factory: HostFactory = Arc::new(move |_| Ok(Arc::clone(&host) as Arc<dyn ModelHost>));
        let events = Arc::new(Mutex::new(Vec::new()));
        let recorded = Arc::clone(&events);
        let broadcast: Broadcast = Arc::new(move |event| recorded.lock().unwrap().push(event));
        let service = LocalProviderService::new(ServiceOptions {
            host_factory,
            bridge_factory: default_bridge_factory(),
            broadcast,
            wait: ms(200),
            retry: ms(10),
            idle: ms(options.idle_ms),
            cleanup_wait: ms(options.cleanup_wait_ms),
            retain_models: options.retain,
        })
        .unwrap();
        let http = crate::http_client_builder().no_proxy().build().unwrap();
        Harness { service, fake, upstream, events, http, releases: Mutex::new(Vec::new()), _server: server }
    }

    pub(crate) fn character_for(definition: &LocalDefinition) -> RuntimeCharacter {
        let profile = &definition.profile;
        RuntimeCharacter {
            id: Some("isolated-validation".into()),
            name: String::new(),
            seat: String::new(),
            backend: profile.backend.clone(),
            model: Some(profile.model.clone()),
            effort: "default".into(),
            fast_mode: false,
            auto_compact_percent: None,
            permission: if profile.backend == "codex" { "read-only" } else { "plan" }.into(),
            identity_prompt: String::new(),
            config: json!({}),
            local_profile: None,
        }
    }

    /// 실행 명세 하나와 그 앞문.
    struct Client {
        launch: LocalAgentLaunch,
        release: EntryRelease,
        url: String,
        key: String,
        codex: bool,
        model: String,
        http: reqwest::Client,
    }

    type Answer = Option<(u16, String)>;

    async fn send(request: reqwest::RequestBuilder) -> Answer {
        let response = request.send().await.ok()?;
        let status = response.status().as_u16();
        let bytes = response.bytes().await.ok()?;
        Some((status, String::from_utf8_lossy(&bytes).into_owned()))
    }

    impl Client {
        fn request(&self, marker: &str, key: &str) -> reqwest::RequestBuilder {
            let body = if self.codex {
                json!({ "model": self.model, "input": marker })
            } else {
                json!({ "model": self.model, "max_tokens": 32, "messages": [{ "role": "user", "content": marker }] })
            };
            self.http.post(&self.url).header("x-api-key", key).body(serde_json::to_vec(&body).unwrap())
        }

        /// 끝까지 받은 상태와 본문. 끊기면 None이다.
        async fn post(&self, marker: &str) -> Answer {
            send(self.request(marker, &self.key)).await
        }

        async fn status(&self, marker: &str) -> u16 {
            self.post(marker).await.expect("front door reply").0
        }

        /// AbortController로 끊을 수 있는 요청. JoinHandle::abort가 연결을 끊는다.
        fn spawn_post(&self, marker: &str) -> JoinHandle<Answer> {
            tokio::spawn(send(self.request(marker, &self.key)))
        }
    }

    async fn cancel(handle: JoinHandle<Answer>) {
        handle.abort();
        let _ = handle.await;
    }

    impl Harness {
        async fn launch_with(&self, definition: &LocalDefinition, mode: &str, attachments: &[StagedAttachment]) -> Result<Client, LocalError> {
            let profile = &definition.profile;
            let character = character_for(definition);
            let paths = LaunchPaths::default();
            let executable = format!("/test/{}", profile.backend);
            let launched = self
                .service
                .launch(&character, definition, LaunchRequest {
                    mode,
                    prompt: "hello",
                    attachments,
                    previous_session_id: None,
                    workdir: "/tmp",
                    base_environment: None,
                    hook_path: "/tmp/test-hook",
                    executable: &executable,
                    paths: &paths,
                })
                .await?;
            self.releases.lock().unwrap().push(launched.release.clone());
            let codex = profile.backend == "codex";
            let endpoint = launched.launch.profile.endpoint.clone();
            let key = if codex { launched.launch.env[&profile.credential_env].clone() } else { launched.launch.env["ANTHROPIC_API_KEY"].clone() };
            Ok(Client {
                url: format!("{endpoint}{}", if codex { "/v1/responses" } else { "/v1/messages" }),
                key,
                codex,
                model: profile.model.clone(),
                http: self.http.clone(),
                release: launched.release,
                launch: launched.launch,
            })
        }

        async fn launch(&self, definition: &LocalDefinition, mode: &str) -> Client {
            self.launch_with(definition, mode, &[]).await.unwrap_or_else(|error| panic!("launch failed: {error}"))
        }

        /// (starts, releases, requests).
        fn counts(&self) -> (usize, usize, usize) {
            (self.fake.starts.load(Ordering::SeqCst), self.fake.releases.load(Ordering::SeqCst), self.upstream.requests.lock().unwrap().len())
        }

        fn requests(&self) -> Vec<(String, Value)> {
            self.upstream.requests.lock().unwrap().clone()
        }

        fn events(&self) -> Vec<Value> {
            self.events.lock().unwrap().clone()
        }

        /// t.after(). 받은 실행을 모두 돌려주고 서비스를 닫는다.
        pub(crate) async fn finish(&self) {
            let releases: Vec<EntryRelease> = self.releases.lock().unwrap().drain(..).collect();
            for release in releases {
                release.release().await.unwrap();
            }
            self.service.shutdown().await.unwrap();
        }
    }

    /// local-provider-service 시험의 LM Studio 정의.
    pub(crate) fn definition_value() -> Value {
        json!({
            "profile": { "id": "local-test", "providerKind": "local", "backend": "claude", "model": "test-model", "endpoint": "http://127.0.0.1:41235",
                "credentialEnv": "OFFICESTRA_LOCAL_TEST_TOKEN", "credentialVersion": "v1", "contextWindow": 32768, "maxOutputTokens": 4096,
                "usageProtocol": "anthropic-normalized-v1" },
            "host": { "address": "127.0.0.1", "user": "test", "sshPort": 2222, "keyPath": "/tmp/test-key", "hostKeyAlias": "test-host", "modelKey": "test/model", "comfyPort": 8188 },
        })
    }

    fn changed(mut value: Value, change: impl FnOnce(&mut Value)) -> LocalDefinition {
        change(&mut value);
        normalize_local_definition(&value).unwrap()
    }

    fn definition() -> LocalDefinition {
        changed(definition_value(), |_| {})
    }

    /// setup(). 실행 하나를 미리 연다(기본 터미널).
    async fn setup(options: Options, mode: &str) -> (Harness, Client) {
        let harness = harness(options);
        let client = harness.launch(&definition(), mode).await;
        (harness, client)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn direct_claude_front_door_keeps_messages_cache_semantics() {
        let reply: Reply = Arc::new(|_, _| {
            json!({ "type": "message", "stop_reason": "end_turn", "content": [{ "type": "text", "text": "ok" }],
                "usage": { "input_tokens": 4, "cache_read_input_tokens": 53, "output_tokens": 20 } })
        });
        let harness = harness(Options { vram: 70.0, ram: 30.0, reply, ..Options::default() });
        let direct = changed(definition_value(), |value| {
            let profile = &mut value["profile"];
            for (key, field) in [("id", json!("claude-direct")), ("runtime", json!(DIRECT_RUNTIME)), ("model", json!("officestra-qwen38-27b-uncensored-q4km")),
                ("contextWindow", json!(65536)), ("kvCacheQuantization", json!("q8_0")), ("reasoning", json!("medium"))] {
                profile[key] = field;
            }
        });
        for mode in ["gui", "terminal"] {
            let client = harness.launch(&direct, mode).await;
            let base = client.launch.env["ANTHROPIC_BASE_URL"].clone();
            let token = client.launch.env["ANTHROPIC_AUTH_TOKEN"].clone();
            let body = json!({ "model": direct.profile.model, "messages": [{ "role": "user", "content": "hi" }], "max_tokens": 512 });
            let (status, text) = send(harness.http.post(format!("{base}/v1/messages")).header("x-api-key", token).body(serde_json::to_vec(&body).unwrap())).await.unwrap();
            assert_eq!(status, 200, "{mode}: {text}");
            let reply: Value = serde_json::from_str(&text).unwrap();
            assert_eq!(reply["usage"]["input_tokens"], 4);
            assert_eq!(reply["usage"]["cache_read_input_tokens"], 53);
            assert_eq!(harness.requests().last().unwrap().1["chat_template_kwargs"], json!({ "reasoning_effort": "medium" }));
            client.release.release().await.unwrap();
        }
        assert!(!local_resume_compatible(&definition().to_value(), &direct.to_value()));
        harness.finish().await;
    }

    /// 인자 `name=JSON` 값을 읽는다.
    fn json_arg(args: &[String], matches: impl Fn(&str) -> bool) -> Value {
        let arg = args.iter().find(|arg| matches(arg)).expect("argument");
        serde_json::from_str(&arg[arg.find('=').unwrap() + 1..]).unwrap()
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn local_codex_uses_authenticated_responses_front_door() {
        let reply: Reply = Arc::new(|_, _| json!({ "id": "response-test", "status": "completed", "output": [], "usage": { "input_tokens": 12, "output_tokens": 2, "total_tokens": 14 } }));
        let harness = harness(Options { vram: 70.0, ram: 30.0, reply, ..Options::default() });
        let codex = changed(definition_value(), |value| {
            let profile = &mut value["profile"];
            profile["id"] = json!("local-codex-test");
            profile["backend"] = json!("codex");
            profile["usageProtocol"] = json!("openai-responses-v1");
        });
        assert!(!local_resume_compatible(&definition().to_value(), &codex.to_value()));
        let client = harness.launch(&codex, "gui").await;
        let catalog_path = json_arg(&client.launch.args, |arg| arg.starts_with("model_catalog_json="));
        let catalog_path = PathBuf::from(catalog_path.as_str().unwrap());
        let pinned: Value = serde_json::from_slice(&std::fs::read(&catalog_path).unwrap()).unwrap();
        assert_eq!(pinned["models"][0]["slug"], "test-model");
        assert_eq!(pinned["models"][0]["supports_search_tool"], true);
        let base = json_arg(&client.launch.args, |arg| arg.starts_with("model_providers.") && arg.contains(".base_url="));
        let base = base.as_str().unwrap().to_string();
        let token = client.launch.env["OFFICESTRA_LOCAL_TEST_TOKEN"].clone();
        assert!(!token.is_empty());
        let bearer = format!("Bearer {token}");
        let (status, text) = send(harness.http.get(format!("{base}/models?client_version=0.154.0")).header("authorization", &bearer)).await.unwrap();
        assert_eq!(status, 200);
        let catalog: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(catalog["models"].as_array().unwrap().len(), 1);
        assert_eq!(catalog["models"][0]["slug"], "test-model");
        assert_eq!(catalog["models"][0]["context_window"], 32768);
        assert_eq!(catalog["models"][0]["service_tiers"], json!([]));
        assert_eq!(send(harness.http.get(format!("{base}/models"))).await.unwrap().0, 401);
        assert_eq!(harness.counts().2, 0);
        let post = |body: Value| send(harness.http.post(format!("{base}/responses")).header("authorization", &bearer).body(serde_json::to_vec(&body).unwrap()));
        let (status, text) = post(json!({ "model": "test-model", "input": "hello" })).await.unwrap();
        assert_eq!(status, 200, "{text}");
        assert_eq!(serde_json::from_str::<Value>(&text).unwrap()["usage"]["input_tokens"], 12);
        let requests = harness.requests();
        assert_eq!(requests[0].0, "/v1/responses");
        assert!(requests[0].1.get("max_output_tokens").is_none());
        assert_eq!(post(json!({ "model": "test-model", "input": "hello", "max_output_tokens": 32769 })).await.unwrap().0, 400);
        assert_eq!(harness.counts().2, 1);
        assert_eq!(send(harness.http.post(format!("{base}/messages")).header("authorization", &bearer).body("{}")).await.unwrap().0, 404);
        client.release.release().await.unwrap();
        assert_eq!(std::fs::metadata(&catalog_path).unwrap_err().kind(), io::ErrorKind::NotFound);
        harness.finish().await;
    }

    impl Harness {
        /// 응답을 다 받은 뒤에도 요청 작업은 잠금을 풀며 정리하는 중일 수 있다. Node는 응답 끝과 정리가 같은 차례에 끝난다.
        async fn quiet(&self) {
            for _ in 0..400 {
                let busy = self.service.0.entries().iter().any(|entry| {
                    let state = entry.lock();
                    state.active.is_some() || !state.pending.is_empty()
                });
                if !busy {
                    return;
                }
                delay(5).await;
            }
            panic!("local requests did not settle");
        }
    }

    /// release:async()=>{entered.resolve();await finish.promise;}
    fn gated_release(entered: &Gate, finish: &Gate) -> Hook {
        let (entered, finish) = (entered.clone(), finish.clone());
        hook(move || {
            let (entered, finish) = (entered.clone(), finish.clone());
            async move {
                entered.open();
                finish.wait().await;
                Ok(())
            }
        })
    }

    fn bare_service(host_factory: HostFactory) -> LocalProviderService {
        LocalProviderService::new(ServiceOptions {
            host_factory,
            bridge_factory: default_bridge_factory(),
            broadcast: Arc::new(|_| {}),
            wait: ms(200),
            retry: ms(10),
            idle: ms(10_000),
            cleanup_wait: ms(15_000),
            retain_models: true,
        })
        .unwrap()
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn idle_profile_cleanup_uses_verified_route_and_never_interrupts_users() {
        let harness = harness(Options::default());
        let client = harness.launch(&definition(), "gui").await;
        assert_eq!(client.status("PRIME").await, 200);
        harness.quiet().await;
        let address = "192.0.2.10";
        let error = harness.service.close_idle_profile("local-test", Some(address)).await.unwrap_err();
        assert!(error.message.contains("사용 중"), "{}", error.message);
        assert!(harness.fake.reconnects.lock().unwrap().is_empty());
        assert_eq!(harness.service.status().len(), 1);
        client.release.release().await.unwrap();
        harness.service.close_idle_profile("local-test", Some(address)).await.unwrap();
        assert_eq!(*harness.fake.reconnects.lock().unwrap(), [address]);
        assert!(harness.service.status().is_empty());
        assert_eq!(harness.counts().1, 1);
        harness.finish().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn live_32k_entry_cannot_be_reused_as_64k() {
        let (harness, _client) = setup(Options::default(), "terminal").await;
        let larger = changed(definition_value(), |value| value["profile"]["contextWindow"] = json!(65536));
        let error = harness.launch_with(&larger, "gui", &[]).await.err().unwrap();
        assert!(error.message.contains("Close existing"), "{}", error.message);
        harness.finish().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn terminal_cancel_waits_for_cleanup_and_queued_input_cancels_independently() {
        let (received, cleaning, release) = (Gate::new(), Gate::new(), Gate::new());
        let options = Options { on_request: Some(hang_at(1, &received)), on_release: Some(gated_release(&cleaning, &release)), ..Options::default() };
        let (harness, client) = setup(options, "terminal").await;
        let first = client.spawn_post("FIRST");
        received.wait().await;
        let queued = client.spawn_post("QUEUED");
        delay(20).await;
        assert_eq!(harness.counts().2, 1);
        cancel(queued).await;
        cancel(first).await;
        cleaning.wait().await;
        let next = client.spawn_post("NEXT");
        delay(20).await;
        assert_eq!(harness.counts().2, 1);
        release.open();
        assert_eq!(next.await.unwrap().unwrap().0, 200);
        let (starts, _, requests) = harness.counts();
        assert_eq!((requests, starts), (2, 2));
        harness.finish().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn gui_next_launch_waits_for_cancelled_cleanup_without_replay() {
        let (received, cleaning, release) = (Gate::new(), Gate::new(), Gate::new());
        let options = Options { on_request: Some(hang_at(1, &received)), on_release: Some(gated_release(&cleaning, &release)), ..Options::default() };
        let (harness, client) = setup(options, "gui").await;
        let first = client.spawn_post("FIRST");
        received.wait().await;
        client.release.release().await.unwrap();
        cleaning.wait().await;
        let definition = definition();
        let pending = harness.launch(&definition, "gui");
        tokio::pin!(pending);
        assert!(tokio::time::timeout(ms(20), &mut pending).await.is_err(), "launch must wait for cleanup");
        release.open();
        let next = pending.await;
        let _ = first.await;
        assert_eq!(client.status("AGAIN").await, 200);
        next.release.release().await.unwrap();
        assert_eq!(harness.counts().2, 2);
        harness.finish().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn cleanup_timeout_and_cancelled_waiter_do_not_start_inference() {
        let (received, cleaning, release) = (Gate::new(), Gate::new(), Gate::new());
        let options = Options { cleanup_wait_ms: 50, on_request: Some(hang_at(1, &received)), on_release: Some(gated_release(&cleaning, &release)), ..Options::default() };
        let (harness, client) = setup(options, "terminal").await;
        let first = client.spawn_post("FIRST");
        received.wait().await;
        cancel(first).await;
        cleaning.wait().await;
        let waiter = client.spawn_post("WAITER");
        delay(10).await;
        cancel(waiter).await;
        let (status, text) = client.post("TIMEOUT").await.unwrap();
        assert_eq!(status, 503);
        assert!(text.contains("cleanup"), "{text}");
        assert_eq!(harness.counts().2, 1);
        release.open();
        delay(20).await;
        assert_eq!(client.status("NEXT").await, 200);
        assert_eq!(harness.counts().2, 2);
        harness.finish().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn unconfirmed_resource_release_fails_closed() {
        let received = Gate::new();
        let broken = Arc::new(AtomicBool::new(true));
        let failing = Arc::clone(&broken);
        let on_release = hook(move || {
            let broken = failing.load(Ordering::SeqCst);
            async move { if broken { Err(LocalError::plain("release failed")) } else { Ok(()) } }
        });
        let (harness, client) = setup(Options { on_request: Some(hang_at(1, &received)), on_release: Some(on_release), ..Options::default() }, "terminal").await;
        let first = client.spawn_post("FIRST");
        received.wait().await;
        cancel(first).await;
        delay(30).await;
        assert_eq!(client.status("NEXT").await, 503);
        assert_eq!(harness.counts().0, 1);
        broken.store(false, Ordering::SeqCst);
        harness.finish().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn lazy_startup_authenticates_normalizes_usage_and_unloads_on_release() {
        let (harness, client) = setup(Options::default(), "terminal").await;
        assert_eq!(harness.counts().0, 0);
        assert_eq!(send(client.request("WRONG", "wrong")).await.unwrap().0, 401);
        assert_eq!(harness.counts().0, 0);
        let (status, text) = client.post("hello").await.unwrap();
        assert_eq!(status, 200, "{text}");
        let body: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(body["usage"]["input_tokens"], 80);
        assert_eq!(body["usage"]["cache_read_input_tokens"], 20);
        client.release.release().await.unwrap();
        assert_eq!(harness.counts(), (1, 1, 1));
        assert!(harness.service.status().is_empty());
        harness.finish().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn production_budget_accepts_gpu_96_12() {
        let (harness, client) = setup(Options { vram: 96.12, ..Options::default() }, "terminal").await;
        assert_eq!(client.status("hello").await, 200);
        harness.finish().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn production_budget_accepts_ram_77_86_and_stops_at_90() {
        let (normal, client) = setup(Options { vram: 96.05, ram: 77.86, ..Options::default() }, "terminal").await;
        assert_eq!(client.status("hello").await, 200);
        let (full, rejected) = setup(Options { vram: 96.05, ram: 90.0, ..Options::default() }, "terminal").await;
        assert_eq!(rejected.status("hello").await, 503);
        assert_eq!(full.counts().2, 0);
        normal.finish().await;
        full.finish().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn busy_host_waits_then_resumes_without_duplicate_inference() {
        let (harness, client) = setup(Options { busy: 2, ..Options::default() }, "terminal").await;
        assert_eq!(client.status("hello").await, 200);
        let (starts, _, requests) = harness.counts();
        assert_eq!((starts, requests), (3, 1));
        assert!(harness.events().iter().any(|event| event["state"] == "waiting"));
        harness.finish().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn idle_unload_keeps_terminal_address_and_reloads() {
        let (harness, client) = setup(Options { idle_ms: 40, ..Options::default() }, "terminal").await;
        let url = client.launch.env["ANTHROPIC_BASE_URL"].clone();
        assert_eq!(client.status("FIRST").await, 200);
        delay(100).await;
        assert_eq!(harness.counts().1, 1);
        assert_eq!(client.status("SECOND").await, 200);
        assert_eq!(url, format!("http://127.0.0.1:{}", harness.service.0.entries()[0].port));
        assert_eq!(harness.counts().0, 2);
        harness.finish().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn lost_inference_is_not_replayed_and_next_request_recovers() {
        let (harness, client) = setup(Options { fail: true, ..Options::default() }, "terminal").await;
        let _ = client.post("LOST").await;
        assert_eq!(harness.counts().2, 1);
        assert_eq!(harness.service.status()[0]["state"], "error");
        assert_eq!(client.status("NEXT").await, 200);
        assert_eq!(harness.counts().2, 2);
        harness.finish().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn closing_waiting_session_cancels_provisioning() {
        let (harness, client) = setup(Options { busy: 999, ..Options::default() }, "terminal").await;
        let pending = client.spawn_post("WAITING");
        delay(25).await;
        client.release.release().await.unwrap();
        let _ = pending.await;
        assert_eq!(harness.counts().2, 0);
        assert!(harness.service.status().is_empty());
        harness.finish().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn meromero_31b_image_attachments_fail_before_loading_gpu() {
        let service = bare_service(Arc::new(|_| Err(LocalError::plain("Must not load GPU"))));
        let attachment = StagedAttachment { name: "image.webp".into(), path: "/tmp/image.webp".into(), directory: "/tmp".into(), is_codex_image: false };
        let pattern = Regex::new("31B.*텍스트 전용.*26B").unwrap();
        let paths = LaunchPaths::default();
        for backend in ["claude", "codex"] {
            for mode in ["gui", "terminal"] {
                let definition = mero_definition(MEROMERO_MODEL_ID, MEROMERO_HOST_KEY, backend, 32768);
                let executable = format!("/test/{backend}");
                let request = LaunchRequest {
                    mode,
                    prompt: "",
                    attachments: std::slice::from_ref(&attachment),
                    previous_session_id: None,
                    workdir: "/tmp",
                    base_environment: None,
                    hook_path: "/tmp/test-hook",
                    executable: &executable,
                    paths: &paths,
                };
                let error = service.launch(&character_for(&definition), &definition, request).await.err().unwrap();
                assert!(pattern.is_match(&error.message), "{backend} {mode}: {}", error.message);
            }
        }
        assert!(service.0.entries().is_empty());
        service.shutdown().await.unwrap();
    }

    /// local-model-pool 시험의 정의(MeroMero 31B 직접 실행 64K).
    fn pool_definition() -> LocalDefinition {
        normalize_local_definition(&shared_definition_value()).unwrap()
    }

    /// fixture()의 launch(backend, mode, reasoning). 같은 모델을 Claude·Codex가 나눠 쓴다.
    fn shared(backend: &str, reasoning: &str) -> LocalDefinition {
        changed(shared_definition_value(), |value| {
            let profile = &mut value["profile"];
            profile["id"] = json!(format!("shared-{backend}"));
            profile["backend"] = json!(backend);
            profile["reasoning"] = json!(reasoning);
            profile["usageProtocol"] = json!(if backend == "claude" { "anthropic-normalized-v1" } else { "openai-responses-v1" });
        })
    }

    fn claude() -> LocalDefinition {
        shared("claude", "off")
    }

    fn codex() -> LocalDefinition {
        shared("codex", "on")
    }

    /// n번째 요청에서 entered를 열고, finish가 열린 뒤에 응답한다.
    fn pause_at(target: usize, entered: &Gate, finish: &Gate) -> OnRequest {
        let (entered, finish) = (entered.clone(), finish.clone());
        on_request(move |count| {
            let (entered, finish) = (entered.clone(), finish.clone());
            async move {
                if count == target {
                    entered.open();
                    finish.wait().await;
                }
                true
            }
        })
    }

    fn idle_ok() -> Hook {
        hook(|| async { Ok(()) })
    }

    fn release_fails_while(broken: &Arc<AtomicBool>, message: &'static str) -> Hook {
        let broken = Arc::clone(broken);
        hook(move || {
            let broken = broken.load(Ordering::SeqCst);
            async move { if broken { Err(LocalError::plain(message)) } else { Ok(()) } }
        })
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn gui_and_terminal_share_one_server_while_requests_and_thinking_stay_separate() {
        let harness = harness(Options::pool());
        let mut clients = Vec::new();
        for mode in ["gui", "terminal"] {
            for definition in [claude(), codex()] {
                clients.push(harness.launch(&definition, mode).await);
            }
        }
        let posts: Vec<_> = clients.iter().enumerate().map(|(index, client)| client.spawn_post(&format!("PRIVATE_{index}"))).collect();
        for (index, post) in posts.into_iter().enumerate() {
            let (status, text) = post.await.unwrap().unwrap();
            assert_eq!(status, 200, "{text}");
            assert!(text.contains(&format!("PRIVATE_{index}")), "{text}");
            for other in (0..4).filter(|other| *other != index) {
                assert!(!text.contains(&format!("PRIVATE_{other}")), "{text}");
            }
        }
        assert_eq!(harness.counts().0, 1);
        assert_eq!(harness.counts().1, 0);
        assert_eq!(harness.upstream.max_inflight.load(Ordering::SeqCst), 1);
        assert!(harness.events().iter().any(|event| event["error"] == "같은 모델의 앞선 요청을 기다리는 중"));
        for (url, body) in harness.requests() {
            assert_eq!(body["chat_template_kwargs"], json!({ "enable_thinking": url == "/v1/responses" }), "{url}");
        }
        let thinking = harness.launch(&shared("claude", "on"), "gui").await;
        assert_eq!(thinking.status("THINK_ON").await, 200);
        assert_eq!(harness.requests().last().unwrap().1["chat_template_kwargs"]["enable_thinking"], true);
        assert_eq!(harness.counts().0, 1);
        harness.finish().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn fifo_waiter_cancellation_never_stops_another_profile_generation() {
        let (entered, finish) = (Gate::new(), Gate::new());
        let harness = harness(Options { on_request: Some(pause_at(1, &entered, &finish)), ..Options::pool() });
        let a = harness.launch(&claude(), "terminal").await;
        let b = harness.launch(&codex(), "terminal").await;
        let first = a.spawn_post("FIRST");
        entered.wait().await;
        let queued = b.spawn_post("CANCEL_ME");
        delay(20).await;
        cancel(queued).await;
        b.release.release().await.unwrap();
        assert_eq!(harness.counts().2, 1);
        assert_eq!(harness.counts().1, 0);
        finish.open();
        assert_eq!(first.await.unwrap().unwrap().0, 200);
        let b2 = harness.launch(&codex(), "terminal").await;
        assert_eq!(b2.status("SECOND").await, 200);
        assert_eq!(harness.counts().0, 1);
        a.release.release().await.unwrap();
        assert_eq!(harness.counts().1, 0);
        assert_eq!(b2.status("THIRD").await, 200);
        b2.release.release().await.unwrap();
        assert_eq!(harness.counts().1, 1);
        harness.finish().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn cancelling_active_inference_releases_before_queued_peer_reloads() {
        let (entered, cleaning, finish) = (Gate::new(), Gate::new(), Gate::new());
        let harness = harness(Options { on_request: Some(hang_at(3, &entered)), on_release: Some(gated_release(&cleaning, &finish)), ..Options::pool() });
        let a = harness.launch(&claude(), "gui").await;
        let b = harness.launch(&codex(), "gui").await;
        assert_eq!(a.status("PRIME_A").await, 200);
        assert_eq!(b.status("PRIME_B").await, 200);
        let first = a.spawn_post("CANCEL_ACTIVE");
        entered.wait().await;
        let next = b.spawn_post("NEXT");
        delay(20).await;
        cancel(first).await;
        cleaning.wait().await;
        assert_eq!(harness.counts().0, 1);
        assert_eq!(harness.counts().2, 3);
        finish.open();
        assert_eq!(next.await.unwrap().unwrap().0, 200);
        assert_eq!(harness.counts().0, 2);
        assert_eq!(harness.counts().2, 4);
        harness.finish().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn shared_idle_unload_waits_for_active_peer_and_keeps_endpoints() {
        let (entered, finish) = (Gate::new(), Gate::new());
        let harness = harness(Options { idle_ms: 80, on_request: Some(pause_at(2, &entered, &finish)), ..Options::pool() });
        let a = harness.launch(&claude(), "terminal").await;
        let b = harness.launch(&codex(), "terminal").await;
        assert_eq!(a.status("IDLE_SOON").await, 200);
        let second = b.spawn_post("GENERATING");
        entered.wait().await;
        delay(150).await;
        assert_eq!(harness.counts().1, 0);
        finish.open();
        assert_eq!(second.await.unwrap().unwrap().0, 200);
        delay(180).await;
        assert_eq!(harness.counts().1, 1);
        assert_eq!(a.status("RELOAD").await, 200);
        assert_eq!(harness.counts().0, 2);
        harness.finish().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn explicit_start_clears_failed_bridge_only_after_cleanup_succeeds() {
        let broken = Arc::new(AtomicBool::new(true));
        let harness = harness(Options { retain: true, on_release: Some(release_fails_while(&broken, "SSH failed")), ..Options::pool() });
        let a = harness.launch(&claude(), "gui").await;
        assert_eq!(a.status("BEFORE").await, 200);
        harness.quiet().await;
        let entry = Arc::clone(&harness.service.0.entries()[0]);
        let bridge = entry.lock().bridge.clone().unwrap();
        bridge.stop("SSH failed").await;
        entry.lock().cleanup_failed = true;
        assert!(!bridge.status().cleanup_complete);
        let raw = entry.definition.to_value();
        let error = harness.service.control_model(&raw, "start", None).await.unwrap_err();
        assert!(error.message.contains("SSH failed"), "{}", error.message);
        assert_eq!(harness.counts().0, 1, "No replacement until cleanup is confirmed");
        broken.store(false, Ordering::SeqCst);
        harness.service.control_model(&raw, "start", None).await.unwrap();
        {
            let state = entry.lock();
            assert!(!state.cleanup_failed);
            assert!(state.bridge.is_none());
        }
        assert_eq!(a.status("AFTER").await, 200);
        assert_eq!(harness.counts().0, 2);
        harness.finish().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn confirmed_cancellation_keeps_one_loaded_server_for_gui_and_terminal() {
        let (reached, verified) = (Gate::new(), Gate::new());
        let checks = Arc::new(AtomicUsize::new(0));
        let (counting, waiting) = (Arc::clone(&checks), verified.clone());
        let wait_for_idle = hook(move || {
            counting.fetch_add(1, Ordering::SeqCst);
            let waiting = waiting.clone();
            async move {
                waiting.wait().await;
                Ok(())
            }
        });
        let harness = harness(Options { on_request: Some(hang_at(1, &reached)), wait_for_idle: Some(wait_for_idle), ..Options::pool() });
        let a = harness.launch(&claude(), "gui").await;
        let b = harness.launch(&codex(), "terminal").await;
        let first = a.spawn_post("CANCEL");
        reached.wait().await;
        let next = b.spawn_post("NEXT");
        cancel(first).await;
        a.release.release().await.unwrap();
        delay(20).await;
        assert_eq!(harness.counts().2, 1);
        assert_eq!(checks.load(Ordering::SeqCst), 1);
        assert_eq!(harness.counts().1, 0);
        verified.open();
        assert_eq!(next.await.unwrap().unwrap().0, 200);
        let a2 = harness.launch(&claude(), "gui").await;
        assert_eq!(a2.status("GUI_RESUME").await, 200);
        assert_eq!(harness.counts().0, 1);
        assert_eq!(harness.counts().1, 0);
        harness.finish().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn cancelled_sole_bridge_stays_warm_until_idle_then_unloads_once() {
        let reached = Gate::new();
        let hold = reached.clone();
        let hang = on_request(move |_| {
            let hold = hold.clone();
            async move {
                hold.open();
                false
            }
        });
        let harness = harness(Options { idle_ms: 60, on_request: Some(hang), wait_for_idle: Some(idle_ok()), ..Options::pool() });
        let a = harness.launch(&claude(), "gui").await;
        let first = a.spawn_post("CANCEL");
        reached.wait().await;
        cancel(first).await;
        delay(20).await;
        assert_eq!(harness.counts().1, 0);
        delay(100).await;
        assert_eq!(harness.counts().1, 1);
        harness.service.shutdown().await.unwrap();
        assert_eq!(harness.counts().1, 1);
        harness.finish().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn unconfirmed_cancellation_unloads_and_reloads_before_new_request() {
        let reached = Gate::new();
        let still = hook(|| async { Err(LocalError::plain("still processing")) });
        let harness = harness(Options { on_request: Some(hang_at(1, &reached)), wait_for_idle: Some(still), ..Options::pool() });
        let a = harness.launch(&claude(), "gui").await;
        let first = a.spawn_post("CANCEL");
        reached.wait().await;
        cancel(first).await;
        assert_eq!(a.status("NEXT").await, 200);
        assert_eq!(harness.counts().0, 2);
        assert_eq!(harness.counts().1, 1);
        harness.finish().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn resident_direct_model_survives_gui_retirement_and_terminal_close() {
        let harness = harness(Options { idle_ms: 40, retain: true, ..Options::pool() });
        let a = harness.launch(&claude(), "gui").await;
        assert_eq!(a.status("FIRST").await, 200);
        a.release.release().await.unwrap();
        delay(100).await;
        assert!(harness.service.status().is_empty());
        assert_eq!(harness.counts().1, 0);
        let b = harness.launch(&codex(), "terminal").await;
        assert_eq!(b.status("SECOND").await, 200);
        b.release.release().await.unwrap();
        delay(100).await;
        assert_eq!(harness.counts().0, 1);
        assert_eq!(harness.counts().1, 0);
        harness.service.shutdown().await.unwrap();
        assert_eq!(harness.counts().1, 1);
        harness.finish().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn explicit_profile_cleanup_finds_resident_model_after_front_door_retires() {
        let harness = harness(Options { idle_ms: 30, retain: true, ..Options::pool() });
        let a = harness.launch(&claude(), "gui").await;
        assert_eq!(a.status("FIRST").await, 200);
        a.release.release().await.unwrap();
        delay(80).await;
        assert!(harness.service.status().is_empty());
        assert_eq!(harness.counts().1, 0);
        harness.service.close_idle_profile("shared-claude", None).await.unwrap();
        assert_eq!(harness.counts().1, 1);
        harness.service.shutdown().await.unwrap();
        assert_eq!(harness.counts().1, 1);
        harness.finish().await;
    }

    fn state_of(service: &LocalProviderService, definition: &LocalDefinition) -> Value {
        service.model_status(definition).unwrap()
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn manual_start_preloads_and_stop_unloads_until_explicit_restart() {
        let harness = harness(Options { retain: true, ..Options::pool() });
        let definition = pool_definition();
        let raw = definition.to_value();
        assert_eq!(harness.service.control_model(&raw, "start", None).await.unwrap()["loaded"], true);
        let peer = shared("codex", "on");
        assert_eq!(state_of(&harness.service, &peer)["state"], "ready");
        assert_eq!(harness.counts().2, 0);
        let a = harness.launch(&claude(), "gui").await;
        let b = harness.launch(&codex(), "terminal").await;
        assert_eq!(a.status("A").await, 200);
        assert_eq!(b.status("B").await, 200);
        assert_eq!(harness.counts().0, 1);
        assert_eq!(harness.service.control_model(&raw, "stop", None).await.unwrap()["state"], "stopped");
        assert_eq!(state_of(&harness.service, &peer)["state"], "stopped");
        assert_eq!(harness.counts().1, 1);
        for client in [&a, &b] {
            assert_eq!(client.status("BLOCKED").await, 503);
        }
        let error = harness.launch_with(&claude(), "gui", &[]).await.err().unwrap();
        assert!(error.message.contains("모델 실행"), "{}", error.message);
        assert_eq!(harness.counts().0, 1);
        assert_eq!(harness.counts().2, 2);
        harness.service.control_model(&raw, "start", None).await.unwrap();
        for client in [&a, &b] {
            assert_eq!(client.status("RESUMED").await, 200);
        }
        assert_eq!(harness.counts().0, 2);
        harness.finish().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn unload_frees_retained_model_without_blocking_next_request() {
        let harness = harness(Options { retain: true, ..Options::pool() });
        let definition = pool_definition();
        let a = harness.launch(&claude(), "gui").await;
        assert_eq!(a.status("FIRST").await, 200);
        assert_eq!(state_of(&harness.service, &definition)["loaded"], true);
        let status = harness.service.control_model(&definition.to_value(), "unload", None).await.unwrap();
        assert_eq!(status["loaded"], false);
        assert_ne!(status["state"], "stopped");
        assert_eq!(harness.counts().1, 1);
        assert_eq!(a.status("AGAIN").await, 200);
        assert_eq!(harness.counts().0, 2);
        harness.finish().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn stop_cancels_active_and_queued_requests_and_reports_unloaded_after_release() {
        let (reached, entered, finish) = (Gate::new(), Gate::new(), Gate::new());
        let options = Options {
            retain: true,
            wait_for_idle: Some(idle_ok()),
            on_request: Some(hang_at(1, &reached)),
            on_release: Some(gated_release(&entered, &finish)),
            ..Options::pool()
        };
        let harness = harness(options);
        let definition = pool_definition();
        let a = harness.launch(&claude(), "gui").await;
        let b = harness.launch(&codex(), "terminal").await;
        let first = a.spawn_post("ACTIVE");
        reached.wait().await;
        let queued = b.spawn_post("QUEUED");
        delay(20).await;
        let (service, raw) = (harness.service.clone(), definition.to_value());
        let stop = tokio::spawn(async move { service.control_model(&raw, "stop", None).await });
        entered.wait().await;
        assert_eq!(state_of(&harness.service, &definition)["state"], "stopping");
        let error = harness.service.control_model(&definition.to_value(), "start", None).await.unwrap_err();
        assert!(error.message.contains("처리 중"), "{}", error.message);
        finish.open();
        stop.await.unwrap().unwrap();
        let _ = first.await;
        let _ = queued.await;
        assert_eq!(harness.counts().2, 1);
        assert_eq!(harness.counts().1, 1);
        let status = state_of(&harness.service, &definition);
        assert_eq!(status["loaded"], false);
        assert_eq!(status["state"], "stopped");
        harness.finish().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn failed_memory_release_is_an_error_never_stopped_or_restarted() {
        let failing = hook(|| async { Err(LocalError::plain("release failed")) });
        let harness = harness(Options { retain: true, on_release: Some(failing), ..Options::pool() });
        let definition = pool_definition();
        let raw = definition.to_value();
        harness.service.control_model(&raw, "start", None).await.unwrap();
        let error = harness.service.control_model(&raw, "stop", None).await.unwrap_err();
        assert!(error.message.contains("release failed"), "{}", error.message);
        assert_eq!(state_of(&harness.service, &definition)["state"], "error");
        let error = harness.service.control_model(&raw, "start", None).await.unwrap_err();
        assert!(error.message.contains("release failed"), "{}", error.message);
        assert_eq!(harness.counts().1, 2, "A new start retries cleanup, but never starts over an unconfirmed process");
        let error = harness.service.shutdown().await.unwrap_err();
        assert!(error.message.contains("release failed"), "{}", error.message);
    }

    /// start({signal}). 중단될 때까지 기다렸다가 실패한다.
    struct AbortingHost(Gate);

    impl ModelHost for AbortingHost {
        fn start(self: Arc<Self>, options: StartOptions) -> BoxFuture<'static, Result<Arc<dyn HostResource>, LocalError>> {
            Box::pin(async move {
                self.0.open();
                options.signal.aborted().await;
                Err(LocalError::plain("start aborted"))
            })
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn shutdown_cancels_pending_manual_start_and_waits_for_cleanup() {
        let entered = Gate::new();
        let host = Arc::new(AbortingHost(entered.clone()));
        let service = bare_service(Arc::new(move |_| Ok(Arc::clone(&host) as Arc<dyn ModelHost>)));
        let definition = pool_definition();
        let (control, raw) = (service.clone(), definition.to_value());
        let started = tokio::spawn(async move { control.control_model(&raw, "start", None).await });
        entered.wait().await;
        let error = service.close_idle_profile(&definition.profile.id, None).await.unwrap_err();
        assert!(error.message.contains("처리 중"), "{}", error.message);
        service.shutdown().await.unwrap();
        let error = started.await.unwrap().unwrap_err();
        assert!(error.message.contains("start aborted"), "{}", error.message);
        assert!(service.0.lock().model_controls.is_empty());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn model_control_rejects_unknown_actions_and_release_modes() {
        let harness = harness(Options::pool());
        let raw = pool_definition().to_value();
        for (action, mode, message) in [
            ("restart", None, "Unsupported model action"),
            ("stop", Some("free"), "Unsupported ComfyUI release mode"),
            ("start", Some("reboot"), "Unsupported ComfyUI release mode"),
        ] {
            let error = harness.service.control_model(&raw, action, mode).await.unwrap_err();
            assert_eq!(error.message, message);
        }
        assert_eq!(harness.counts().0, 0);
        harness.finish().await;
    }
}
