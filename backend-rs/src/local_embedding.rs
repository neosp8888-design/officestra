// 로컬 bge-m3 임베딩. 모델 파일 준비, 필요할 때만 띄우는 워커 프로세스 관리, 워커 본체를 담는다. Node local-embedding*.mjs를 옮겼다.

use std::collections::{BTreeMap, HashMap};
use std::io::{self, BufRead, Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::time::Duration;

use futures_util::FutureExt;
use futures_util::future::{BoxFuture, Shared};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, oneshot};
use tokio::task::AbortHandle;

use crate::js::{js_string, js_string_or_empty, js_to_number, js_trim, js_truthy};

pub const MODEL_REPOSITORY: &str = "onnx-community/bge-m3-ONNX";
pub const MODEL_REVISION: &str = "25b9af8e87a38eb120cfe87125383677b9cd309e";
pub const MODEL_DTYPE: &str = "q8";
pub const DIMENSIONS: usize = 1024;
/// 업무 기록은 요청이 제목에, 최종 결과가 본문 끝에 있어 두 구간을 우선한다. M4 CPU에서 긴 self-attention 백필이 폭증하지 않게 192토큰으로 자른다.
pub const MAX_TOKENS: usize = 192;
pub const IDLE_TIMEOUT: Duration = Duration::from_millis(5 * 60 * 1000);
pub const REQUEST_TIMEOUT: Duration = Duration::from_millis(3 * 60 * 1000);
/// 저장소@리비전@dtype. rag_documents.embedding_model에 그대로 들어간다.
pub const MODEL_ID: &str = "onnx-community/bge-m3-ONNX@25b9af8e87a38eb120cfe87125383677b9cd309e@q8";
const MANIFEST_FILE: &str = "OFFICESTRA-MODEL.json";
const SERVICE_CLOSED: &str = "로컬 임베딩 서비스를 종료했습니다.";
const WORKER_STOPPED: &str = "로컬 임베딩 워커를 종료했습니다.";

pub struct ModelFile {
    pub path: &'static str,
    pub size: u64,
    pub sha256: &'static str,
}

pub const MODEL_FILES: [ModelFile; 4] = [
    ModelFile {
        path: "config.json",
        size: 658,
        sha256: "70dae5884ced999af00244f776ac9eaa71538d68497d3d6a6091e0318cd32905",
    },
    ModelFile {
        path: "tokenizer_config.json",
        size: 1203,
        sha256: "b87c8703482b0300d3da30e201519aa641f6a450f5eb5bf1e624afbf70c74d80",
    },
    ModelFile {
        path: "tokenizer.json",
        size: 17_082_799,
        sha256: "249df0778f236f6ece390de0de746838ef25b9d6954b68c2ee71249e0a9d8fd4",
    },
    ModelFile {
        path: "onnx/model_quantized.onnx",
        size: 568_479_395,
        sha256: "2237f770aad5c71bbc1fc2d361a57f9a37400574cc9eff32626f0cdb49234730",
    },
];

/// Node의 `environment = process.env`. 워커에는 이 값 전체에 두 변수를 더해 넘긴다.
pub type Environment = BTreeMap<String, String>;

pub fn process_environment() -> Environment {
    std::env::vars_os()
        .filter_map(|(key, value)| Some((key.into_string().ok()?, value.into_string().ok()?)))
        .collect()
}

/// localEmbeddingModelDirectory. 지정값은 다듬기만 하고 상대경로도 그대로 쓴다.
pub fn model_directory(environment: &Environment) -> PathBuf {
    let configured = js_trim(environment.get("OFFICE_EMBEDDING_MODEL_DIR").map_or("", String::as_str));
    if !configured.is_empty() {
        return PathBuf::from(configured);
    }
    std::env::home_dir()
        .unwrap_or_default()
        .join(".officestra")
        .join("models")
        .join(format!("bge-m3-onnx-{}", &MODEL_REVISION[..12]))
}

pub struct ModelReady {
    pub directory: PathBuf,
    pub downloaded: Vec<String>,
}

fn hex_sha256(path: &Path) -> io::Result<String> {
    let mut file = std::fs::File::open(path)?;
    let mut hash = Sha256::new();
    let mut buffer = vec![0; 1 << 20];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hash.update(&buffer[..read]);
    }
    Ok(hash.finalize().iter().map(|byte| format!("{byte:02x}")).collect())
}

/// validFile. 없으면 false, 그 밖의 읽기 오류는 그대로 실패다.
fn valid_file(path: &Path, expected: &ModelFile) -> Result<bool, String> {
    let checked = std::fs::metadata(path).and_then(|metadata| {
        if !metadata.is_file() || metadata.len() != expected.size {
            return Ok(false);
        }
        Ok(hex_sha256(path)? == expected.sha256)
    });
    match checked {
        Ok(valid) => Ok(valid),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.to_string()),
    }
}

/// validManifest. 없거나 JSON이 아니면 false다. JSON null은 Node처럼 속성 읽기 오류로 실패한다.
fn valid_manifest(directory: &Path) -> Result<bool, String> {
    let bytes = match std::fs::read(directory.join(MANIFEST_FILE)) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error.to_string()),
    };
    let Ok(parsed) = serde_json::from_str::<Value>(&String::from_utf8_lossy(&bytes)) else {
        return Ok(false);
    };
    if parsed.is_null() {
        return Err("Cannot read properties of null (reading 'modelID')".into());
    }
    let text = |key: &str| parsed.get(key).and_then(Value::as_str);
    Ok(text("modelID") == Some(MODEL_ID)
        && text("sourceRepository") == Some(MODEL_REPOSITORY)
        && text("revision") == Some(MODEL_REVISION)
        && text("license") == Some("MIT")
        && MODEL_FILES.iter().all(|file| {
            let entry = parsed.get("files").and_then(|files| files.get(file.path));
            entry.and_then(|entry| entry.get("sha256")).and_then(Value::as_str) == Some(file.sha256)
                && entry.and_then(|entry| entry.get("size")).and_then(Value::as_f64) == Some(file.size as f64)
        }))
}

async fn blocking<T: Send + 'static>(work: impl FnOnce() -> Result<T, String> + Send + 'static) -> Result<T, String> {
    tokio::task::spawn_blocking(work).await.map_err(|error| error.to_string())?
}

async fn download_file(file: &'static ModelFile, destination: PathBuf) -> Result<(), String> {
    let url = format!("https://huggingface.co/{MODEL_REPOSITORY}/resolve/{MODEL_REVISION}/{}?download=true", file.path);
    let client = crate::http_client_builder().build().map_err(|error| error.to_string())?;
    let mut response = client.get(&url).send().await.map_err(|error| error.to_string())?;
    if !response.status().is_success() {
        return Err(format!("임베딩 모델 파일을 받지 못했습니다. {} HTTP {}", file.path, response.status().as_u16()));
    }
    let temporary = std::env::temp_dir().join(format!(
        "officestra-embedding-{}-{}.partial",
        std::process::id(),
        file.path.replace('/', "-")
    ));
    let result = async {
        let mut output = tokio::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&temporary)
            .await
            .map_err(|error| error.to_string())?;
        while let Some(chunk) = response.chunk().await.map_err(|error| error.to_string())? {
            output.write_all(&chunk).await.map_err(|error| error.to_string())?;
        }
        output.flush().await.map_err(|error| error.to_string())?;
        drop(output);
        let (checked, target) = (temporary.clone(), destination.clone());
        blocking(move || {
            if !valid_file(&checked, file)? {
                return Err(format!("임베딩 모델 파일 검증에 실패했습니다. {}", file.path));
            }
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
            }
            std::fs::copy(&checked, &target).map(drop).map_err(|error| error.to_string())
        })
        .await
    }
    .await;
    let _ = tokio::fs::remove_file(&temporary).await;
    result
}

/// ensureLocalEmbeddingModel. 기록표와 해시가 맞으면 그대로 쓰고, 아니면 없는 파일만 받은 뒤 기록표를 새로 쓴다.
pub async fn ensure_local_embedding_model(environment: &Environment) -> Result<ModelReady, String> {
    let directory = model_directory(environment);
    let checked = directory.clone();
    let complete = blocking(move || {
        std::fs::create_dir_all(&checked).map_err(|error| error.to_string())?;
        if !valid_manifest(&checked)? {
            return Ok(false);
        }
        for file in &MODEL_FILES {
            if !valid_file(&checked.join(file.path), file)? {
                return Ok(false);
            }
        }
        Ok(true)
    })
    .await?;
    if complete {
        return Ok(ModelReady { directory, downloaded: Vec::new() });
    }

    let mut downloaded = Vec::new();
    for file in &MODEL_FILES {
        let destination = directory.join(file.path);
        let target = destination.clone();
        if blocking(move || valid_file(&target, file)).await? {
            continue;
        }
        if environment.get("OFFICE_EMBEDDING_ALLOW_MODEL_DOWNLOAD").map(String::as_str) == Some("0") {
            return Err(format!("로컬 임베딩 모델 파일이 없습니다. {}", file.path));
        }
        download_file(file, destination).await?;
        downloaded.push(file.path.to_string());
    }
    let files: Map<String, Value> = MODEL_FILES
        .iter()
        .map(|file| (file.path.to_string(), json!({ "size": file.size, "sha256": file.sha256 })))
        .collect();
    let manifest = json!({
        "modelID": MODEL_ID,
        "sourceRepository": MODEL_REPOSITORY,
        "revision": MODEL_REVISION,
        "format": "ONNX Q8",
        "license": "MIT",
        "files": files,
    });
    let text = format!("{}\n", serde_json::to_string_pretty(&manifest).map_err(|error| error.to_string())?);
    let path = directory.join(MANIFEST_FILE);
    blocking(move || {
        std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&path)
            .and_then(|mut output| output.write_all(text.as_bytes()))
            .map_err(|error| error.to_string())
    })
    .await?;
    Ok(ModelReady { directory, downloaded })
}

/// 워커에 보내는 요청. delivered는 Node `worker.send`의 콜백 자리다.
pub struct WorkerRequest {
    pub id: String,
    pub texts: Vec<String>,
    pub delivered: oneshot::Sender<Result<(), String>>,
}

pub enum WorkerEvent {
    Message(Value),
    /// Node의 exit·error 이벤트. 값은 대기 중 요청에 그대로 돌려줄 오류 문구다.
    Exit(String),
}

pub struct WorkerHandle {
    pub requests: mpsc::UnboundedSender<WorkerRequest>,
    pub events: mpsc::UnboundedReceiver<WorkerEvent>,
    pub kill: Box<dyn FnMut() + Send>,
}

pub type ModelLoader = Arc<dyn Fn(Environment) -> BoxFuture<'static, Result<PathBuf, String>> + Send + Sync>;
pub type WorkerFactory = Arc<dyn Fn(&Path, &Environment) -> Result<WorkerHandle, String> + Send + Sync>;
pub type Logger = Arc<dyn Fn(&str) + Send + Sync>;

pub struct EmbeddingOptions {
    pub idle_timeout: Duration,
    pub request_timeout: Duration,
    pub worker_factory: WorkerFactory,
    pub model_loader: ModelLoader,
    pub environment: Environment,
    /// Node의 logger.warn. 임베딩 비활성 사유만 여기로 간다.
    pub logger: Logger,
}

impl Default for EmbeddingOptions {
    fn default() -> Self {
        Self {
            idle_timeout: IDLE_TIMEOUT,
            request_timeout: REQUEST_TIMEOUT,
            worker_factory: match std::env::current_exe() {
                Ok(executable) => process_worker_factory(executable),
                Err(error) => {
                    let message = error.to_string();
                    Arc::new(move |_, _| Err(message.clone()))
                }
            },
            model_loader: default_model_loader(),
            environment: process_environment(),
            logger: Arc::new(|message| eprintln!("{message}")),
        }
    }
}

pub fn default_model_loader() -> ModelLoader {
    Arc::new(|environment| {
        async move { ensure_local_embedding_model(&environment).await.map(|ready| ready.directory) }.boxed()
    })
}

/// 같은 실행 파일을 `embedding-worker` 하위 명령으로 띄운다. Node의 fork와 같은 자리다.
pub fn process_worker_factory(executable: PathBuf) -> WorkerFactory {
    Arc::new(move |directory, environment| spawn_process_worker(&executable, directory, environment))
}

type ModelReadyFuture = Shared<BoxFuture<'static, Result<PathBuf, String>>>;

struct ActiveWorker {
    generation: u64,
    requests: mpsc::UnboundedSender<WorkerRequest>,
    kill: Box<dyn FnMut() + Send>,
}

struct Pending {
    sender: oneshot::Sender<Result<Vec<Value>, String>>,
    timeout: AbortHandle,
}

#[derive(Default)]
struct State {
    worker: Option<ActiveWorker>,
    generation: u64,
    pending: HashMap<String, Pending>,
    idle_timer: Option<AbortHandle>,
    peak_worker_rss_bytes: f64,
    last_worker_rss_bytes: f64,
    model_ready: Option<ModelReadyFuture>,
    last_unavailable_reason: Option<String>,
    closed: bool,
}

struct Inner {
    options: EmbeddingOptions,
    state: Mutex<State>,
    next_request: AtomicU64,
}

/// LocalEmbeddingService. 워커는 첫 요청 때 만들고 유휴 5분 뒤 내린다. 복제본은 같은 워커를 공유한다.
#[derive(Clone)]
pub struct LocalEmbeddingService {
    inner: Arc<Inner>,
}

impl Default for LocalEmbeddingService {
    fn default() -> Self {
        Self::new(EmbeddingOptions::default())
    }
}

/// `Number(value) || 0`.
fn number_or_zero(value: Option<&Value>) -> f64 {
    let number = value.map_or(f64::NAN, js_to_number);
    if number.is_nan() { 0.0 } else { number + 0.0 }
}

impl LocalEmbeddingService {
    pub fn new(options: EmbeddingOptions) -> Self {
        Self { inner: Arc::new(Inner { options, state: Mutex::default(), next_request: AtomicU64::new(1) }) }
    }

    pub fn model_id(&self) -> &'static str {
        MODEL_ID
    }

    pub fn has_active_worker(&self) -> bool {
        self.inner.lock().worker.is_some()
    }

    pub fn metrics(&self) -> Value {
        let state = self.inner.lock();
        json!({
            "active": state.worker.is_some(),
            "peakWorkerRSSBytes": state.peak_worker_rss_bytes,
            "lastWorkerRSSBytes": state.last_worker_rss_bytes,
        })
    }

    /// embed. 다듬은 값은 검사에만 쓰고 워커에는 원문을 보낸다.
    pub async fn embed(&self, texts: &[String]) -> Result<Vec<Value>, String> {
        if self.inner.lock().closed {
            return Err(SERVICE_CLOSED.into());
        }
        if texts.is_empty() || texts.iter().any(|text| js_trim(text).is_empty()) {
            return Err("임베딩할 비어 있지 않은 텍스트가 필요합니다.".into());
        }
        let (generation, requests) = self.ensure_worker().await?;
        let id = self.inner.next_request.fetch_add(1, Ordering::Relaxed).to_string();
        let (sender, receiver) = oneshot::channel();
        {
            let mut state = self.inner.lock();
            if let Some(timer) = state.idle_timer.take() {
                timer.abort();
            }
            let (weak, timeout_id, limit) = (Arc::downgrade(&self.inner), id.clone(), self.inner.options.request_timeout);
            let timeout = tokio::spawn(async move {
                tokio::time::sleep(limit).await;
                if let Some(inner) = weak.upgrade() {
                    inner.request_timed_out(&timeout_id, generation);
                }
            })
            .abort_handle();
            state.pending.insert(id.clone(), Pending { sender, timeout });
        }
        let (delivered, delivery) = oneshot::channel();
        // 받는 쪽이 닫혀 있으면 요청과 함께 delivered도 버려져 아래에서 전송 실패로 처리된다.
        let _ = requests.send(WorkerRequest { id: id.clone(), texts: texts.to_vec(), delivered });
        let weak = Arc::downgrade(&self.inner);
        tokio::spawn(async move {
            let error = match delivery.await {
                Ok(Ok(())) => return,
                Ok(Err(error)) => error,
                Err(_) => "Channel closed".to_string(),
            };
            if let Some(inner) = weak.upgrade() {
                inner.send_failed(generation, &id, error);
            }
        });
        receiver.await.unwrap_or_else(|_| Err(WORKER_STOPPED.into()))
    }

    async fn ensure_worker(&self) -> Result<(u64, mpsc::UnboundedSender<WorkerRequest>), String> {
        let inner = &self.inner;
        let ready = {
            let mut state = inner.lock();
            if let Some(worker) = &state.worker {
                return Ok((worker.generation, worker.requests.clone()));
            }
            state
                .model_ready
                .get_or_insert_with(|| (inner.options.model_loader)(inner.options.environment.clone()).shared())
                .clone()
        };
        let result = ready.await;
        let mut state = inner.lock();
        let directory = match result {
            Ok(directory) => directory,
            Err(error) => {
                // 파일을 나중에 복구하거나 받을 수 있으므로 다음 요청은 준비를 다시 시도한다.
                state.model_ready = None;
                inner.report_unavailable(&mut state, &error);
                return Err(error);
            }
        };
        if state.closed {
            return Err(SERVICE_CLOSED.into());
        }
        if let Some(worker) = &state.worker {
            return Ok((worker.generation, worker.requests.clone()));
        }
        let handle = match (inner.options.worker_factory)(&directory, &inner.options.environment) {
            Ok(handle) => handle,
            Err(error) => {
                inner.report_unavailable(&mut state, &error);
                return Err(error);
            }
        };
        state.generation += 1;
        let generation = state.generation;
        let WorkerHandle { requests, mut events, kill } = handle;
        state.worker = Some(ActiveWorker { generation, requests: requests.clone(), kill });
        let weak = Arc::downgrade(inner);
        tokio::spawn(async move {
            while let Some(event) = events.recv().await {
                let Some(inner) = weak.upgrade() else { return };
                match event {
                    WorkerEvent::Message(message) => inner.handle_message(generation, &message),
                    WorkerEvent::Exit(error) => {
                        inner.handle_worker_exit(generation, &error);
                        return;
                    }
                }
            }
        });
        Ok((generation, requests))
    }

    pub fn close(&self) {
        let mut state = self.inner.lock();
        state.closed = true;
        if let Some(generation) = state.worker.as_ref().map(|worker| worker.generation) {
            self.inner.stop_worker(&mut state, generation);
        }
    }
}

impl Inner {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn report_unavailable(&self, state: &mut State, reason: &str) {
        if state.last_unavailable_reason.as_deref() == Some(reason) {
            return;
        }
        state.last_unavailable_reason = Some(reason.to_string());
        (self.options.logger)(&format!("임베딩 비활성(사유): {reason}"));
    }

    fn is_current(state: &State, generation: u64) -> bool {
        state.worker.as_ref().is_some_and(|worker| worker.generation == generation)
    }

    fn handle_message(self: &Arc<Self>, generation: u64, message: &Value) {
        let mut state = self.lock();
        if !Self::is_current(&state, generation) {
            return;
        }
        let Some(pending) = message.get("id").and_then(Value::as_str).and_then(|id| state.pending.remove(id)) else {
            return;
        };
        pending.timeout.abort();
        if js_truthy(message.get("ok")) {
            let result = message.get("result").filter(|result| !result.is_null());
            state.last_worker_rss_bytes = number_or_zero(result.and_then(|result| result.get("rssBytes")));
            state.peak_worker_rss_bytes = state
                .peak_worker_rss_bytes
                .max(number_or_zero(result.and_then(|result| result.get("maxRSSBytes"))))
                .max(state.last_worker_rss_bytes);
            state.last_unavailable_reason = None;
            let vectors = match result.and_then(|result| result.get("vectors")) {
                Some(Value::Array(vectors)) => vectors.clone(),
                _ => Vec::new(),
            };
            let _ = pending.sender.send(Ok(vectors));
            self.schedule_idle_release(&mut state);
        } else {
            let error = match message.get("error") {
                Some(error) if js_truthy(Some(error)) => js_string(error),
                _ => "로컬 임베딩에 실패했습니다.".to_string(),
            };
            self.report_unavailable(&mut state, &error);
            let _ = pending.sender.send(Err(error));
            self.stop_worker(&mut state, generation);
        }
    }

    fn handle_worker_exit(&self, generation: u64, error: &str) {
        let mut state = self.lock();
        if !Self::is_current(&state, generation) {
            return;
        }
        state.worker = None;
        self.report_unavailable(&mut state, error);
        if let Some(timer) = state.idle_timer.take() {
            timer.abort();
        }
        reject_all(&mut state, error);
    }

    fn schedule_idle_release(self: &Arc<Self>, state: &mut State) {
        if let Some(timer) = state.idle_timer.take() {
            timer.abort();
        }
        let (weak, limit): (Weak<Self>, Duration) = (Arc::downgrade(self), self.options.idle_timeout);
        state.idle_timer = Some(
            tokio::spawn(async move {
                tokio::time::sleep(limit).await;
                if let Some(inner) = weak.upgrade() {
                    let mut state = inner.lock();
                    if let Some(generation) = state.worker.as_ref().map(|worker| worker.generation) {
                        inner.stop_worker(&mut state, generation);
                    }
                }
            })
            .abort_handle(),
        );
    }

    fn stop_worker(&self, state: &mut State, generation: u64) {
        let Some(mut worker) = state.worker.take_if(|worker| worker.generation == generation) else {
            return;
        };
        if let Some(timer) = state.idle_timer.take() {
            timer.abort();
        }
        (worker.kill)();
        reject_all(state, WORKER_STOPPED);
    }

    fn request_timed_out(&self, id: &str, generation: u64) {
        let mut state = self.lock();
        let Some(pending) = state.pending.remove(id) else { return };
        let error = "로컬 임베딩 응답 시간이 초과됐습니다.";
        self.report_unavailable(&mut state, error);
        let _ = pending.sender.send(Err(error.into()));
        self.stop_worker(&mut state, generation);
    }

    fn send_failed(&self, generation: u64, id: &str, error: String) {
        let mut state = self.lock();
        let Some(pending) = state.pending.remove(id) else { return };
        pending.timeout.abort();
        self.report_unavailable(&mut state, &error);
        let _ = pending.sender.send(Err(error));
        self.stop_worker(&mut state, generation);
    }
}

fn reject_all(state: &mut State, error: &str) {
    for (_, pending) in state.pending.drain() {
        pending.timeout.abort();
        let _ = pending.sender.send(Err(error.to_string()));
    }
}

pub(crate) fn signal_name(signal: i32) -> String {
    let name = match signal {
        libc::SIGHUP => "SIGHUP",
        libc::SIGINT => "SIGINT",
        libc::SIGQUIT => "SIGQUIT",
        libc::SIGILL => "SIGILL",
        libc::SIGTRAP => "SIGTRAP",
        libc::SIGABRT => "SIGABRT",
        libc::SIGBUS => "SIGBUS",
        libc::SIGFPE => "SIGFPE",
        libc::SIGKILL => "SIGKILL",
        libc::SIGSEGV => "SIGSEGV",
        libc::SIGPIPE => "SIGPIPE",
        libc::SIGALRM => "SIGALRM",
        libc::SIGTERM => "SIGTERM",
        _ => return signal.to_string(),
    };
    name.to_string()
}

/// 워커 프로세스. 요청·응답은 stdin·stdout의 JSON 한 줄씩이고 stderr는 백엔드 로그로 넘긴다.
fn spawn_process_worker(executable: &Path, directory: &Path, environment: &Environment) -> Result<WorkerHandle, String> {
    let mut child = tokio::process::Command::new(executable)
        .arg("embedding-worker")
        .env_clear()
        .envs(environment)
        .env("HF_HUB_DISABLE_TELEMETRY", "1")
        .env("OFFICE_EMBEDDING_MODEL_DIR", directory)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| error.to_string())?;
    let (Some(mut stdin), Some(stdout), Some(mut stderr)) = (child.stdin.take(), child.stdout.take(), child.stderr.take())
    else {
        return Err("로컬 임베딩 워커 입출력을 열지 못했습니다.".into());
    };
    let pid = child.id().and_then(|id| libc::pid_t::try_from(id).ok());
    let (request_sender, mut request_receiver) = mpsc::unbounded_channel::<WorkerRequest>();
    let (event_sender, event_receiver) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        while let Some(request) = request_receiver.recv().await {
            let mut line = serde_json::to_vec(&json!({ "id": request.id, "texts": request.texts })).unwrap_or_default();
            line.push(b'\n');
            let written = async {
                stdin.write_all(&line).await?;
                stdin.flush().await
            }
            .await;
            let _ = request.delivered.send(written.map_err(|error| error.to_string()));
        }
    });
    tokio::spawn(async move {
        let mut buffer = vec![0; 8192];
        while let Ok(read) = stderr.read(&mut buffer).await
            && read > 0
        {
            let text = String::from_utf8_lossy(&buffer[..read]);
            let message = js_trim(&text);
            if !message.is_empty() {
                eprintln!("로컬 임베딩 워커: {message}");
            }
        }
    });
    let exited = Arc::new(AtomicBool::new(false));
    let exit_flag = exited.clone();
    tokio::spawn(async move {
        let mut lines = BufReader::new(stdout).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            if let Ok(message) = serde_json::from_str::<Value>(&line) {
                let _ = event_sender.send(WorkerEvent::Message(message));
            }
        }
        let status = child.wait().await;
        exit_flag.store(true, Ordering::SeqCst);
        let (code, signal) = match status {
            Ok(status) => (
                status.code().map_or_else(|| "null".to_string(), |code| code.to_string()),
                status.signal().map_or_else(|| "null".to_string(), signal_name),
            ),
            Err(_) => ("null".to_string(), "null".to_string()),
        };
        let _ = event_sender.send(WorkerEvent::Exit(format!("로컬 임베딩 워커가 종료됐습니다. code={code} signal={signal}")));
    });
    let kill = Box::new(move || {
        if let Some(pid) = pid
            && !exited.load(Ordering::SeqCst)
        {
            // SAFETY: 이 워커 프로세스의 pid에 신호만 보낸다. 이미 끝났으면 위 표시로 건너뛴다.
            unsafe {
                libc::kill(pid, libc::SIGTERM);
            }
        }
    });
    Ok(WorkerHandle { requests: request_sender, events: event_receiver, kill })
}

/// 워커가 쓸 ONNX Runtime 동적 라이브러리. ORT_DYLIB_PATH, 실행 파일 옆 libonnxruntime.dylib 순서로 찾는다.
fn onnx_runtime_library() -> Result<PathBuf, String> {
    if let Some(path) = std::env::var_os("ORT_DYLIB_PATH").filter(|path| !path.is_empty()) {
        return Ok(PathBuf::from(path));
    }
    if let Some(candidate) = std::env::current_exe()
        .ok()
        .and_then(|executable| executable.parent().map(|directory| directory.join("libonnxruntime.dylib")))
        && candidate.is_file()
    {
        return Ok(candidate);
    }
    Err("ONNX Runtime 라이브러리를 찾을 수 없습니다. ORT_DYLIB_PATH를 지정하세요.".into())
}

struct WorkerModel {
    tokenizer: tokenizers::Tokenizer,
    session: ort::session::Session,
    _environment: ort::environment::Environment,
    pad_token_id: i64,
    end_token_id: u32,
}

fn message(error: impl ToString) -> String {
    error.to_string()
}

impl WorkerModel {
    fn load(directory: &Path) -> Result<Self, String> {
        use ort::session::Session;
        use ort::session::builder::{AutoDevicePolicy, GraphOptimizationLevel};

        // Node 워커처럼 tokenizer_config.json도 읽어 JSON인지 확인한다. bge-m3에서는 인코딩 결과에 영향이 없다.
        let config = std::fs::read_to_string(directory.join("tokenizer_config.json")).map_err(message)?;
        serde_json::from_str::<Value>(&config).map_err(message)?;
        let tokenizer = tokenizers::Tokenizer::from_file(directory.join("tokenizer.json")).map_err(message)?;
        let environment = ort::init_from(onnx_runtime_library()?).map_err(message)?.build().map_err(message)?;
        // ort 기본 정책은 macOS에서 CoreML을 붙인다. Node 워커와 같은 CPU 실행 공급자만 쓰게 고정한다.
        let session = Session::builder(&environment)
            .map_err(message)?
            .with_auto_device(AutoDevicePolicy::PreferCPU)
            .map_err(message)?
            .with_optimization_level(GraphOptimizationLevel::All)
            .map_err(message)?
            .commit_from_file(directory.join("onnx").join("model_quantized.onnx"))
            .map_err(message)?;
        let pad_token_id = i64::from(tokenizer.token_to_id("<pad>").unwrap_or(1));
        let end_token_id = tokenizer.token_to_id("</s>").unwrap_or(2);
        Ok(Self { tokenizer, session, _environment: environment, pad_token_id, end_token_id })
    }

    fn truncated_ids(&self, text: &str) -> Result<Vec<u32>, String> {
        let mut ids = self.tokenizer.encode(text, true).map_err(message)?.get_ids().to_vec();
        if ids.len() > MAX_TOKENS {
            ids.truncate(MAX_TOKENS);
            if let Some(last) = ids.last_mut() {
                *last = self.end_token_id;
            }
        }
        Ok(ids)
    }

    fn embed(&mut self, texts: &[String]) -> Result<Value, String> {
        use ort::value::Tensor;

        let encoded = texts.iter().map(|text| self.truncated_ids(text)).collect::<Result<Vec<_>, _>>()?;
        let sequence_length = encoded.iter().map(Vec::len).max().unwrap_or(0);
        let element_count = texts.len() * sequence_length;
        let mut input_ids = vec![self.pad_token_id; element_count];
        let mut attention_mask = vec![0_i64; element_count];
        for (batch, ids) in encoded.iter().enumerate() {
            let offset = batch * sequence_length;
            for (token, id) in ids.iter().enumerate() {
                input_ids[offset + token] = i64::from(*id);
                attention_mask[offset + token] = 1;
            }
        }
        let shape = [texts.len(), sequence_length];
        let outputs = self
            .session
            .run(ort::inputs! {
                "input_ids" => Tensor::from_array((shape, input_ids)).map_err(message)?,
                "attention_mask" => Tensor::from_array((shape, attention_mask)).map_err(message)?,
            })
            .map_err(message)?;
        let (_, hidden_state) = outputs["last_hidden_state"].try_extract_tensor::<f32>().map_err(message)?;
        let vectors = (0..texts.len())
            .map(|index| normalized_cls(hidden_state, index, sequence_length))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(json!({
            "vectors": vectors,
            "dimensions": DIMENSIONS,
            "rssBytes": resident_bytes(),
            "maxRSSBytes": max_resident_bytes(),
        }))
    }
}

/// CLS 자리(각 문장의 첫 토큰) 은닉 상태를 f64로 L2 정규화한다. 합산 순서까지 Node와 같다.
fn normalized_cls(output: &[f32], batch_index: usize, sequence_length: usize) -> Result<Vec<f64>, String> {
    let offset = batch_index * sequence_length * DIMENSIONS;
    let vector: Vec<f64> =
        output.get(offset..offset + DIMENSIONS).unwrap_or_default().iter().map(|value| f64::from(*value)).collect();
    let magnitude = vector.iter().fold(0.0, |sum, value| sum + value * value).sqrt();
    if !magnitude.is_finite() || magnitude == 0.0 {
        return Err("로컬 임베딩 정규화에 실패했습니다.".into());
    }
    Ok(vector.iter().map(|value| value / magnitude).collect())
}

#[cfg(target_os = "macos")]
fn resident_bytes() -> u64 {
    let mut information: libc::proc_taskinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_taskinfo>() as libc::c_int;
    // SAFETY: 자기 pid의 작업 정보를 크기가 맞는 구조체에 받는다.
    let written = unsafe {
        libc::proc_pidinfo(
            libc::getpid(),
            libc::PROC_PIDTASKINFO,
            0,
            (&raw mut information).cast::<libc::c_void>(),
            size,
        )
    };
    if written == size { information.pti_resident_size } else { 0 }
}

#[cfg(not(target_os = "macos"))]
fn resident_bytes() -> u64 {
    0
}

/// 최대 상주 메모리(바이트). macOS의 ru_maxrss는 바이트, 그 밖은 KiB다.
fn max_resident_bytes() -> u64 {
    let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
    // SAFETY: 자기 프로세스 사용량을 구조체에 받는다.
    if unsafe { libc::getrusage(libc::RUSAGE_SELF, &raw mut usage) } != 0 {
        return 0;
    }
    let maximum = u64::try_from(usage.ru_maxrss).unwrap_or(0);
    if cfg!(target_os = "macos") { maximum } else { maximum * 1024 }
}

fn worker_reply(message: &Value, directory: &Path, model: &mut Option<Result<WorkerModel, String>>) -> Value {
    let result = (|| {
        let texts: Vec<String> = match message.get("texts") {
            Some(Value::Array(items)) => items.iter().map(|text| js_string_or_empty(Some(text))).collect(),
            _ => Vec::new(),
        };
        if texts.is_empty() {
            return Err("임베딩할 텍스트가 없습니다.".to_string());
        }
        // Node의 modelPromise처럼 첫 준비 결과(실패 포함)를 계속 쓴다.
        let model = model.get_or_insert_with(|| WorkerModel::load(directory)).as_mut().map_err(|error| error.clone())?;
        model.embed(&texts)
    })();
    let mut reply = Map::new();
    if let Some(id) = message.get("id") {
        reply.insert("id".into(), id.clone());
    }
    match result {
        Ok(result) => {
            reply.insert("ok".into(), Value::Bool(true));
            reply.insert("result".into(), result);
        }
        Err(error) => {
            reply.insert("ok".into(), Value::Bool(false));
            reply.insert("error".into(), Value::String(error));
        }
    }
    Value::Object(reply)
}

/// `embedding-worker` 하위 명령의 본체. 요청을 한 줄씩 받아 차례로 처리하고, 입력이 닫히면 끝낸다.
pub fn run_worker() -> i32 {
    let directory = std::env::var_os("OFFICE_EMBEDDING_MODEL_DIR").unwrap_or_default();
    if directory.is_empty() {
        eprintln!("Error: OFFICE_EMBEDDING_MODEL_DIR가 필요합니다.");
        return 1;
    }
    let directory = PathBuf::from(directory);
    let mut model = None;
    let mut stdout = io::stdout().lock();
    for line in io::stdin().lock().lines() {
        let Ok(line) = line else { break };
        let message = serde_json::from_str::<Value>(&line).unwrap_or(Value::Null);
        let reply = worker_reply(&message, &directory, &mut model);
        let written = serde_json::to_writer(&mut stdout, &reply)
            .map_err(io::Error::from)
            .and_then(|()| stdout.write_all(b"\n"))
            .and_then(|()| stdout.flush());
        if written.is_err() {
            break;
        }
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 모델_식별자와_운영_기본값은_node와_같다() {
        assert_eq!(MODEL_ID, [MODEL_REPOSITORY, MODEL_REVISION, MODEL_DTYPE].join("@"));
        assert_eq!(IDLE_TIMEOUT, Duration::from_millis(300_000));
        assert_eq!(REQUEST_TIMEOUT, Duration::from_millis(180_000));
    }

    #[test]
    fn 모델_폴더는_지정값을_다듬어_쓰고_없으면_홈_아래_리비전_폴더다() {
        let configured = Environment::from([("OFFICE_EMBEDDING_MODEL_DIR".to_string(), "  ./models  ".to_string())]);
        assert_eq!(model_directory(&configured), PathBuf::from("./models"));
        let blank = Environment::from([("OFFICE_EMBEDDING_MODEL_DIR".to_string(), " \n".to_string())]);
        let expected = std::env::home_dir().unwrap().join(".officestra/models/bge-m3-onnx-25b9af8e87a3");
        assert_eq!(model_directory(&blank), expected);
        assert_eq!(model_directory(&Environment::new()), expected);
    }

    #[test]
    fn 정규화는_f64로_합산하고_영벡터는_실패한다() {
        let mut output = vec![0.0_f32; DIMENSIONS * 2];
        output[0] = 3.0;
        output[1] = 4.0;
        let vector = normalized_cls(&output, 0, 1).unwrap();
        assert_eq!((vector[0], vector[1], vector[2]), (0.6, 0.8, 0.0));
        assert_eq!(normalized_cls(&output, 1, 1).unwrap_err(), "로컬 임베딩 정규화에 실패했습니다.");
    }

    #[test]
    fn 워커는_빈_요청에_id를_돌려주며_실패를_알린다() {
        let mut model = None;
        let reply = worker_reply(&json!({ "id": "a", "texts": [] }), Path::new("/없는-폴더"), &mut model);
        assert_eq!(reply, json!({ "id": "a", "ok": false, "error": "임베딩할 텍스트가 없습니다." }));
        assert!(model.is_none());
        let reply = worker_reply(&json!({ "texts": "문장" }), Path::new("/없는-폴더"), &mut model);
        assert_eq!(serde_json::to_string(&reply).unwrap(), r#"{"ok":false,"error":"임베딩할 텍스트가 없습니다."}"#);
    }

    #[test]
    fn 기록표는_필수_항목과_파일_해시가_모두_맞아야_한다() {
        let directory = std::env::temp_dir().join(format!("office-rs-manifest-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join(MANIFEST_FILE);
        assert!(!valid_manifest(&directory).unwrap());
        std::fs::write(&path, "{").unwrap();
        assert!(!valid_manifest(&directory).unwrap());
        std::fs::write(&path, "null").unwrap();
        assert_eq!(valid_manifest(&directory).unwrap_err(), "Cannot read properties of null (reading 'modelID')");
        let files: Map<String, Value> =
            MODEL_FILES.iter().map(|file| (file.path.into(), json!({ "size": file.size, "sha256": file.sha256 }))).collect();
        let mut manifest = json!({
            "modelID": MODEL_ID, "sourceRepository": MODEL_REPOSITORY, "revision": MODEL_REVISION,
            "format": "ONNX Q8", "license": "MIT", "files": files,
        });
        std::fs::write(&path, manifest.to_string()).unwrap();
        assert!(valid_manifest(&directory).unwrap());
        manifest["files"]["tokenizer.json"]["size"] = json!(1);
        std::fs::write(&path, manifest.to_string()).unwrap();
        assert!(!valid_manifest(&directory).unwrap());
        std::fs::remove_dir_all(&directory).unwrap();
    }
}
