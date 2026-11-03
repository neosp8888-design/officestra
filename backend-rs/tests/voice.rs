// 음성 따라 읽기 통합 시험. 가짜 백엔드(조회·/ws), 실제 따라 읽기 실행 파일, 가짜 합성 작업자로 Node voice 시험을 옮겼다.
// 실제 음성 합성·재생은 하지 않는다(--dry-run·--no-play·가짜 파이썬).

use std::collections::HashMap;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Query, State};
use axum::http::{Request, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use http_body_util::BodyExt;
use officestra_backend::health::Runtime;
use officestra_backend::http::{AppState, router};
use officestra_backend::js::{iso_string, now_milliseconds};
use officestra_backend::local_embedding::LocalEmbeddingService;
use officestra_backend::voice::feed::{FeedEvent, FeedOptions, FeedSubscription, subscribe_voice_feed};
use officestra_backend::voice::{
    CHANNEL_FD_ENV, FollowStatus, FollowerHandle, SpawnFollower, VoiceFollowControl, process_spawner, read_voice_turn,
};
use officestra_backend::{db, migrate};
use serde_json::{Value, json};
use sqlx::PgPool;
use tokio::sync::OnceCell;
use tokio::sync::mpsc::{UnboundedSender, unbounded_channel};
use tower::ServiceExt;

const FOLLOWER: &str = env!("CARGO_BIN_EXE_officestra-voice-follow");
const WORKER: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/voice/tts_worker.py");

/// 시험마다 따로 쓰는 임시 폴더. 끝나면 지운다.
struct TempDir(String);

impl TempDir {
    fn new(name: &str) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "officestra-voice-{name}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::SeqCst)
        ));
        std::fs::create_dir_all(&path).unwrap();
        Self(path.to_string_lossy().into_owned())
    }

    fn path(&self, relative: &str) -> String {
        format!("{}/{relative}", self.0)
    }

    fn write(&self, relative: &str, content: &str) -> String {
        let path = self.path(relative);
        if let Some(parent) = Path::new(&path).parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&path, content).unwrap();
        path
    }

    /// 실행 권한을 준 스크립트.
    fn script(&self, relative: &str, content: &str) -> String {
        use std::os::unix::fs::PermissionsExt;
        let path = self.write(relative, content);
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    fn read(&self, relative: &str) -> Option<String> {
        std::fs::read_to_string(self.path(relative)).ok()
    }

    fn lock_owner(&self) -> Option<u32> {
        self.read("active-follow.lock/pid")?.trim().parse().ok()
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// 시험이 실패해도 띄운 따라 읽기를 남기지 않는다.
struct Running(Child);

impl Running {
    fn pid(&self) -> u32 {
        self.0.id()
    }

    fn terminate(&mut self) -> std::process::ExitStatus {
        signal(self.pid(), libc::SIGTERM);
        self.0.wait().unwrap()
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        if matches!(self.0.try_wait(), Ok(None)) {
            signal(self.pid(), libc::SIGTERM);
            let _ = self.0.wait();
        }
    }
}

fn signal(pid: u32, signal: i32) {
    // SAFETY: 이 시험이 띄운 자식 번호에만 보낸다.
    unsafe {
        libc::kill(pid as i32, signal);
    }
}

fn alive(pid: u32) -> bool {
    // SAFETY: 신호 0은 존재만 확인한다.
    unsafe { libc::kill(pid as i32, 0) == 0 }
}

async fn sleep(milliseconds: u64) {
    tokio::time::sleep(Duration::from_millis(milliseconds)).await;
}

/// Node until(). 실패 기한만 5초로 넉넉히 둔다(실행 파일 시작 포함).
async fn until(mut predicate: impl FnMut() -> bool) {
    for _ in 0..500 {
        if predicate() {
            return;
        }
        sleep(10).await;
    }
    panic!("condition did not become true");
}

fn until_blocking(mut predicate: impl FnMut() -> bool) {
    for _ in 0..500 {
        if predicate() {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("condition did not become true");
}

/// 가짜 백엔드 상태. Node fixture의 state와 같은 값을 센다.
#[derive(Default)]
struct Feed {
    requests: AtomicUsize,
    connections: AtomicUsize,
    delay: AtomicU64,
    failure: AtomicBool,
    turn: Mutex<Value>,
    /// 직원 ID나 since가 빠진 조회. 시험 끝에 비어 있어야 한다.
    wrong: Mutex<Vec<String>>,
    /// 열린 소켓. Some은 보낼 글, None은 닫기 프레임 없이 끊기(ws terminate)다.
    sockets: Mutex<Vec<UnboundedSender<Option<String>>>>,
}

impl Feed {
    fn requests(&self) -> usize {
        self.requests.load(Ordering::SeqCst)
    }

    fn connections(&self) -> usize {
        self.connections.load(Ordering::SeqCst)
    }

    fn set_turn(&self, turn: Value) {
        *self.turn.lock().unwrap() = turn;
    }

    fn turn(&self) -> Value {
        self.turn.lock().unwrap().clone()
    }

    fn notify(&self, character_id: &str) {
        let text = json!({ "type": "feed.changed", "characterId": character_id }).to_string();
        for socket in self.sockets.lock().unwrap().iter() {
            let _ = socket.send(Some(text.clone()));
        }
    }

    fn terminate_sockets(&self) {
        for socket in self.sockets.lock().unwrap().drain(..) {
            let _ = socket.send(None);
        }
    }

    fn assert_queries(&self) {
        assert!(self.wrong.lock().unwrap().is_empty(), "잘못된 조회 {:?}", self.wrong.lock().unwrap());
    }
}

async fn voice_feed(State(feed): State<Arc<Feed>>, Query(query): Query<HashMap<String, String>>) -> Response {
    feed.requests.fetch_add(1, Ordering::SeqCst);
    let since_present = query.get("since").is_some_and(|since| !since.is_empty());
    if query.get("characterId").map(String::as_str) != Some("right-woman") || !since_present {
        feed.wrong.lock().unwrap().push(format!("{query:?}"));
    }
    // Node처럼 응답 본문은 지연 전에 정한다.
    let body = json!({ "turn": feed.turn() }).to_string();
    sleep(feed.delay.load(Ordering::SeqCst)).await;
    let status = if feed.failure.load(Ordering::SeqCst) { StatusCode::SERVICE_UNAVAILABLE } else { StatusCode::OK };
    (status, [(header::CONTENT_TYPE, "application/json")], body).into_response()
}

async fn feed_socket(State(feed): State<Arc<Feed>>, upgrade: WebSocketUpgrade) -> Response {
    upgrade.on_upgrade(move |socket| serve_socket(feed, socket))
}

async fn serve_socket(feed: Arc<Feed>, mut socket: WebSocket) {
    feed.connections.fetch_add(1, Ordering::SeqCst);
    let (sender, mut commands) = unbounded_channel();
    feed.sockets.lock().unwrap().push(sender);
    loop {
        tokio::select! {
            command = commands.recv() => match command {
                Some(Some(text)) => {
                    if socket.send(Message::Text(text.into())).await.is_err() {
                        return;
                    }
                }
                // 닫기 프레임 없이 연결을 버린다.
                _ => return,
            },
            incoming = socket.recv() => {
                if !matches!(incoming, Some(Ok(_))) {
                    return;
                }
            }
        }
    }
}

/// 조회·/ws만 있는 가짜 백엔드. 주소를 돌려준다.
async fn serve_feed(turn: Value) -> (Arc<Feed>, String) {
    let feed = Arc::new(Feed { turn: Mutex::new(turn), ..Feed::default() });
    let app = Router::new()
        .route("/api/voice-feed", get(voice_feed))
        .route("/ws", get(feed_socket))
        .with_state(Arc::clone(&feed));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (feed, format!("http://{address}"))
}

fn now_iso() -> String {
    iso_string(now_milliseconds())
}

/// Node voice-feed fixture. 첫 동기화로 답변 하나를 받은 구독.
struct Subscribed {
    feed: Arc<Feed>,
    updates: Arc<Mutex<Vec<Value>>>,
    errors: Arc<Mutex<Vec<String>>>,
    subscription: FeedSubscription,
}

impl Subscribed {
    fn updates(&self) -> Vec<Value> {
        self.updates.lock().unwrap().clone()
    }

    fn update_count(&self) -> usize {
        self.updates.lock().unwrap().len()
    }
}

async fn subscribed() -> Subscribed {
    let turn = json!({ "id": "a", "characterId": "right-woman", "startedAt": now_iso(), "response": "첫 문장.", "status": "running" });
    let (feed, backend) = serve_feed(turn).await;
    let (sender, mut receiver) = unbounded_channel();
    let mut options = FeedOptions::new(&backend, "right-woman", &now_iso());
    options.batch = Duration::from_millis(20);
    options.retry = Duration::from_millis(30);
    options.max_retry = Duration::from_millis(120);
    let subscription = subscribe_voice_feed(options, sender).unwrap();
    let updates: Arc<Mutex<Vec<Value>>> = Arc::default();
    let errors: Arc<Mutex<Vec<String>>> = Arc::default();
    let (turns, failures) = (Arc::clone(&updates), Arc::clone(&errors));
    tokio::spawn(async move {
        while let Some(event) = receiver.recv().await {
            match event {
                FeedEvent::Turn(turn) => turns.lock().unwrap().push(turn),
                FeedEvent::Error(message) => failures.lock().unwrap().push(message),
            }
        }
    });
    let subscribed = Subscribed { feed, updates, errors, subscription };
    until(|| subscribed.update_count() == 1).await;
    subscribed
}

#[tokio::test]
async fn idle_waiting_and_other_characters_cause_no_extra_requests() {
    let subscribed = subscribed().await;
    for _ in 0..100 {
        subscribed.feed.notify("boss");
    }
    sleep(1_200).await; // 이전 대기 폴링 주기보다 길게 관찰
    assert_eq!(subscribed.feed.requests(), 1, "연결 시 최초 동기화 이외의 조회 없음");
    assert_eq!(subscribed.update_count(), 1);
    subscribed.feed.assert_queries();
}

#[tokio::test]
async fn notification_bursts_are_batched_and_identical_text_is_not_resent() {
    let subscribed = subscribed().await;
    let feed = &subscribed.feed;
    for _ in 0..100 {
        feed.notify("right-woman");
    }
    until(|| feed.requests() == 2).await;
    sleep(60).await;
    assert_eq!(subscribed.update_count(), 1);
    assert_eq!(feed.requests(), 2);
    let mut turn = feed.turn();
    turn["response"] = json!("첫 문장. 마지막 문장");
    turn["status"] = json!("completed");
    feed.set_turn(turn.clone());
    feed.notify("right-woman");
    until(|| subscribed.update_count() == 2).await;
    let updates = subscribed.updates();
    assert_eq!(updates[1]["status"], "completed");
    assert_eq!(updates[1]["response"], turn["response"]);
    feed.assert_queries();
}

#[tokio::test]
async fn a_change_arriving_during_a_request_is_not_lost() {
    let subscribed = subscribed().await;
    let feed = &subscribed.feed;
    feed.delay.store(100, Ordering::SeqCst);
    feed.notify("right-woman");
    until(|| feed.requests() == 2).await;
    let mut turn = feed.turn();
    turn["id"] = json!("b");
    turn["response"] = json!("새 답변.");
    feed.set_turn(turn);
    feed.notify("right-woman");
    until(|| subscribed.updates().iter().any(|turn| turn["id"] == "b")).await;
    assert_eq!(feed.requests(), 3);
    feed.assert_queries();
}

#[tokio::test]
async fn reconnecting_syncs_once_without_rereading_the_same_reply() {
    let subscribed = subscribed().await;
    let feed = &subscribed.feed;
    feed.terminate_sockets();
    until(|| feed.connections() == 2 && feed.requests() == 2).await;
    sleep(80).await;
    assert_eq!(subscribed.update_count(), 1);
    assert_eq!(feed.requests(), 2);
    assert!(subscribed.errors.lock().unwrap().is_empty(), "끊김은 오류로 알리지 않는다");
    feed.assert_queries();
}

#[tokio::test]
async fn request_errors_retry_and_close_stops_everything() {
    let subscribed = subscribed().await;
    let feed = &subscribed.feed;
    feed.failure.store(true, Ordering::SeqCst);
    feed.notify("right-woman");
    until(|| !subscribed.errors.lock().unwrap().is_empty()).await;
    assert_eq!(subscribed.errors.lock().unwrap()[0], "음성 응답 조회 HTTP 503");
    feed.failure.store(false, Ordering::SeqCst);
    let mut turn = feed.turn();
    turn["response"] = json!("복구된 답변.");
    feed.set_turn(turn);
    until(|| subscribed.update_count() == 2).await;
    subscribed.subscription.close();
    let requests = feed.requests();
    let connections = feed.connections();
    sleep(200).await;
    assert_eq!(feed.requests(), requests);
    assert_eq!(feed.connections(), connections);
    feed.assert_queries();
}

/// 제어기가 띄우는 실행 파일 자리. 실제 따라 읽기를 모델 없이(--dry-run) 띄우고 작업 폴더·인자·번호를 남긴다.
fn dry_run_wrapper(home: &TempDir) -> String {
    home.script(
        "follower.sh",
        &format!(
            "#!/bin/sh\npwd -P > spawned.txt\nfor argument in \"$@\"; do echo \"$argument\" >> spawned.txt; done\necho $$ > child.pid\n\
             OFFICE_VOICE_DIR=\"$(pwd -P)\" exec '{FOLLOWER}' \"$@\" --dry-run --backend http://127.0.0.1:9\n"
        ),
    )
}

fn child_pid(home: &TempDir) -> u32 {
    home.read("child.pid").unwrap().trim().parse().unwrap()
}

/// 시험이 실패해도 번호로만 아는 자식을 남기지 않는다.
struct KillOnDrop(u32);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        if alive(self.0) {
            signal(self.0, libc::SIGTERM);
        }
    }
}

/// spawnSync(…, { timeout: 10_000 }). 표준입력은 닫고 출력은 모은다. 자식 번호도 돌려준다.
fn run_to_end(command: &mut Command) -> (u32, std::process::Output) {
    let child = command.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
    let pid = child.id();
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = sender.send(child.wait_with_output());
    });
    match receiver.recv_timeout(Duration::from_secs(10)) {
        Ok(output) => (pid, output.unwrap()),
        Err(_) => {
            signal(pid, libc::SIGKILL);
            panic!("10초 안에 끝나지 않음");
        }
    }
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

#[tokio::test]
async fn real_child_ipc_reports_ready_and_exit_without_loading_a_model() {
    let home = TempDir::new("control");
    let control = VoiceFollowControl::new(process_spawner(dry_run_wrapper(&home), Some(home.0.clone())));
    assert_eq!(control.set(Some(&json!(true)), Some(&json!("right-woman"))).unwrap().state, "starting");
    until(|| control.status().state != "starting").await;
    let pid = child_pid(&home);
    let _guard = KillOnDrop(pid);
    assert_eq!(control.status(), FollowStatus { character_id: Some("right-woman".into()), state: "running", error: None });
    // 백엔드 작업 폴더가 아니라 음성 폴더에서 '--follow 직원ID'로 띄운다.
    let canonical = std::fs::canonicalize(&home.0).unwrap().to_string_lossy().into_owned();
    assert_eq!(home.read("spawned.txt").unwrap(), format!("{canonical}\n--follow\nright-woman\n"));
    assert_eq!(home.lock_owner(), Some(pid));
    assert_eq!(control.set(Some(&json!(false)), None).unwrap().state, "stopped");
    until(|| !alive(pid)).await;
    assert_eq!(control.status().state, "stopped");
    assert!(!Path::new(&home.path("active-follow.lock")).exists(), "멈출 때 잠금을 놓는다");
    let log = home.read("logs/follow.log").unwrap();
    assert!(log.contains("right-woman의 새 응답을 기다립니다."), "{log}");
}

#[test]
fn the_follower_stops_when_the_backend_goes_away() {
    let home = TempDir::new("disconnect");
    let wrapper = dry_run_wrapper(&home);
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    let control = runtime.block_on(async {
        let control = VoiceFollowControl::new(process_spawner(wrapper, Some(home.0.clone())));
        control.set(Some(&json!(true)), Some(&json!("right-woman"))).unwrap();
        until(|| control.status().state != "starting").await;
        control
    });
    assert_eq!(control.status().state, "running");
    let pid = child_pid(&home);
    let _guard = KillOnDrop(pid);
    // 백엔드 프로세스가 끝난 것처럼 소켓을 쥔 작업을 모두 버린다(Node 'disconnect').
    drop(runtime);
    until_blocking(|| !Path::new(&home.path("active-follow.lock")).exists());
    // 거둘 실행기가 없어졌으니 직접 거둔다. 다른 시험의 실행기가 먼저 거뒀으면 바로 돌아온다.
    let mut status = 0;
    // SAFETY: 이 시험이 띄운 자식만 기다린다.
    unsafe { libc::waitpid(pid as i32, &mut status, 0) };
    assert!(!alive(pid));
}

fn start_dry_run(home: &TempDir) -> Running {
    Running(
        Command::new(FOLLOWER)
            .args(["--dry-run", "--backend", "http://127.0.0.1:9"])
            .env("OFFICE_VOICE_DIR", &home.0)
            .env_remove(CHANNEL_FD_ENV)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    )
}

#[test]
fn only_one_follower_reads_replies_at_a_time() {
    let home = TempDir::new("singleton");
    let mut first = start_dry_run(&home);
    until_blocking(|| home.lock_owner() == Some(first.pid()));
    let (_, second) = run_to_end(
        Command::new(FOLLOWER)
            .args(["--dry-run", "--backend", "http://127.0.0.1:9"])
            .env("OFFICE_VOICE_DIR", &home.0)
            .env_remove(CHANNEL_FD_ENV),
    );
    assert_eq!(second.status.code(), Some(3));
    assert_eq!(text(&second.stderr), format!("음성 따라 읽기가 이미 실행 중입니다 (PID {}).\n", first.pid()));
    assert_eq!(first.terminate().code(), Some(0), "종료 신호는 Node stop처럼 0으로 끝난다");
    assert!(!Path::new(&home.path("active-follow.lock")).exists());
    let third = start_dry_run(&home);
    until_blocking(|| home.lock_owner() == Some(third.pid()));
}

#[tokio::test]
async fn a_dry_run_follower_finishes_on_notifications_and_does_not_reread_after_reconnect() {
    let home = TempDir::new("events");
    let (feed, backend) = serve_feed(Value::Null).await;
    let mut child = Running(
        Command::new(FOLLOWER)
            .args(["--follow", "right-woman", "--dry-run", "--backend", &backend])
            .env("OFFICE_VOICE_DIR", &home.0)
            .env_remove(CHANNEL_FD_ENV)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    until(|| feed.requests() == 1).await;
    sleep(1_100).await;
    assert_eq!(feed.requests(), 1);
    feed.set_turn(json!({
        "id": "stream", "characterId": "right-woman", "startedAt": now_iso(), "status": "running", "response": "첫 문장입니다. ",
    }));
    feed.notify("right-woman");
    until(|| feed.requests() == 2).await;
    sleep(100).await;
    let mut turn = feed.turn();
    turn["status"] = json!("completed");
    turn["response"] = json!("첫 문장입니다. 마지막 문장입니다");
    feed.set_turn(turn);
    feed.notify("right-woman");
    // 쓰는 중인 줄은 다음 번에 다시 읽는다.
    let records = || -> Vec<Value> {
        let Some(content) = home.read("logs/latency.jsonl") else { return Vec::new() };
        content.trim().lines().map(serde_json::from_str).collect::<Result<_, _>>().unwrap_or_default()
    };
    until(|| !records().is_empty()).await;
    let first = records();
    assert_eq!(first.len(), 1);
    assert_eq!(first[0]["sentences"], 2, "완료 알림에서 마침표 없는 마지막 문장도 읽음");
    assert_eq!((first[0]["characterId"].as_str(), first[0]["mode"].as_str()), (Some("right-woman"), Some("follow")));
    let before = feed.requests();
    feed.terminate_sockets();
    until(|| feed.requests() == before + 1).await;
    sleep(200).await;
    assert_eq!(records().len(), 1);
    assert_eq!(child.terminate().code(), Some(0));
    feed.assert_queries();
}

#[test]
fn a_worker_initialisation_error_fails_the_follower() {
    let root = TempDir::new("start");
    let python = root.script("fake-python", "#!/bin/sh\nprintf '{\"ready\":false,\"error\":\"tokenizer unavailable\"}\\n'\nexit 4\n");
    let sample = root.write("sample.md", REPLAY_SAMPLE);
    let (_, output) = run_to_end(
        Command::new(FOLLOWER)
            .args(["--replay", &sample, "--no-play"])
            .current_dir(&root.0)
            .env("OFFICE_VOICE_DIR", &root.0)
            .env("OFFICE_VOICE_PYTHON", &python)
            .env_remove(CHANNEL_FD_ENV),
    );
    assert_eq!(output.status.code(), Some(4), "{}", text(&output.stderr));
    let stdout = text(&output.stdout);
    assert!(stdout.contains("합성 초기화 실패: tokenizer unavailable"), "{stdout}");
    assert!(!stdout.contains("합성 준비 완료"), "{stdout}");
}

#[test]
fn a_model_without_a_tokenizer_reports_not_ready_and_exits() {
    let root = TempDir::new("worker");
    root.write("numpy.py", "");
    root.write("mlx_audio/__init__.py", "");
    root.write("mlx_audio/tts/__init__.py", "");
    root.write(
        "mlx_audio/tts/utils.py",
        "def load_model(_):\n    print(\"Warning: Could not load tokenizer: missing source\")\n    \
         return type(\"Model\", (), {\"sample_rate\": 24000, \"tokenizer\": None})()\n",
    );
    let (_, output) = run_to_end(Command::new("python3").arg(WORKER).current_dir(&root.0).env("PYTHONPATH", &root.0));
    let stderr = text(&output.stderr);
    assert_eq!(output.status.code(), Some(4), "{stderr}");
    assert!(stderr.contains("Could not load tokenizer"), "{stderr}");
    let reply: Value = serde_json::from_str(text(&output.stdout).trim()).unwrap();
    assert_eq!(reply, json!({ "ready": false, "error": "RuntimeError: 텍스트 토크나이저를 불러오지 못했습니다." }));
}

/// 요청 줄을 CAPTURE에 남기고 빈 wav와 짧은 길이를 돌려주는 가짜 합성 작업자.
const FAKE_WORKER: &str = r#"#!/usr/bin/env python3
import json, os, sys
capture = os.environ["CAPTURE"]
print(json.dumps({"ready": True, "model": "fake-model", "loadMs": 12, "sampleRate": 24000}), flush=True)
for line in sys.stdin:
    if not line.strip():
        continue
    with open(capture, "a", encoding="utf-8") as handle:
        handle.write(line)
    request = json.loads(line)
    open(request["out"], "wb").close()
    print(json.dumps({"id": request["id"], "path": request["out"], "synthMs": 3, "audioMs": 20}), flush=True)
"#;

/// 기준 음성 복제(right-woman)와 이름 음성(default) 두 갈래를 모두 쓰는 설정.
const REPLAY_VOICES: &str = r#"{
  "default": { "model": "fake-model", "voice": "Sohee", "instruct": "차분하게", "rate": 1.1 },
  "right-woman": { "model": "fake-model", "refAudio": "refs/base.wav", "refText": "기준 문장", "styles": { "sad": { "refAudio": "refs/sad.wav", "refText": "슬픈 문장" } } },
  "emotion": { "exclaim": "밝게", "question": "묻듯이" }
}
"#;

const REPLAY_SAMPLE: &str = "## 보고\n(울먹이며) 힘든 하루였어요. 정말 고마워요!\n\n다음에 또 봐요?\n(웃음)\n- 수정 파일은 `a/b/c.swift` 입니다.\n";

/// "[HH:MM:SS] " 머리와 실행마다 다른 숫자ms를 가린다.
fn mask_log_line(line: &str) -> String {
    let line = match line.as_bytes() {
        [b'[', _, _, b':', _, _, b':', _, _, b']', b' ', ..] => &line[11..],
        _ => line,
    };
    let mut masked = String::new();
    let mut digits = String::new();
    let mut rest = line;
    while let Some(character) = rest.chars().next() {
        rest = &rest[character.len_utf8()..];
        if character.is_ascii_digit() {
            digits.push(character);
            continue;
        }
        if !digits.is_empty() {
            let unit = character == 'm' && rest.starts_with('s');
            masked.push_str(if unit { "N" } else { &digits });
            digits.clear();
        }
        masked.push(character);
    }
    masked.push_str(&digits);
    masked
}

/// 흘려 읽기를 끝까지 돌려 (요청 줄, 지연 기록, 로그 줄)을 돌려준다. 경로·자식 번호는 가린다.
fn replay(who: &str) -> (Vec<String>, Vec<Value>, Vec<String>, String) {
    let home = TempDir::new("replay");
    home.write("voices.json", REPLAY_VOICES);
    let python = home.script("fake-python", FAKE_WORKER);
    let sample = home.write("sample.md", REPLAY_SAMPLE);
    let (pid, output) = run_to_end(
        Command::new(FOLLOWER)
            .args(["--replay", &sample, "--no-play", "--cps", "2000", "--follow", who])
            .env("OFFICE_VOICE_DIR", &home.0)
            .env("OFFICE_VOICE_PYTHON", &python)
            .env("CAPTURE", home.path("requests.jsonl"))
            .env_remove(CHANNEL_FD_ENV),
    );
    assert_eq!(output.status.code(), Some(0), "{}", text(&output.stderr));
    assert_eq!(text(&output.stderr), "");
    let mut audio_directories = Vec::new();
    let requests = home
        .read("requests.jsonl")
        .unwrap()
        .lines()
        .map(|line| {
            let mut request: Value = serde_json::from_str(line).unwrap();
            let out = request["out"].as_str().unwrap().to_string();
            let id = request["id"].as_str().unwrap();
            let (directory, file) = out.rsplit_once('/').unwrap();
            assert_eq!(file, format!("{pid}-{id}.wav"));
            assert!(directory.rsplit('/').next().unwrap().starts_with("officestra-local-voice-"), "{out}");
            audio_directories.push(directory.to_string());
            request["out"] = json!(format!("<audio>/<pid>-{id}.wav"));
            serde_json::to_string(&request).unwrap().replace(&home.0, "<home>")
        })
        .collect();
    // 임시 음성 폴더는 끝날 때 지운다.
    assert!(audio_directories.iter().all(|directory| !Path::new(directory).exists()));
    let records = home.read("logs/latency.jsonl").unwrap().lines().map(|line| serde_json::from_str(line).unwrap()).collect();
    let log = text(&output.stdout).lines().map(mask_log_line).collect();
    (requests, records, log, sample)
}

/// 아래 기대값은 같은 설정·글·가짜 작업자로 Node voice-follow.mjs를 돌린 결과와 같다(경로·번호만 가림).
const REPLAY_LOG: [&str; 7] = [
    "합성 준비 완료 · fake-model · 적재 Nms · 예열 Nms",
    "합성 Nms → 음성 Nms · 보고",
    "합성 Nms → 음성 Nms [sad] · 힘든 하루였어요.",
    "합성 Nms → 음성 Nms [sad] · 정말 고마워요!",
    "합성 Nms → 음성 Nms · 다음에 또 봐요?",
    "합성 Nms → 음성 Nms [playful] · 수정 파일은 c.swift 입니다.",
    "기록: 첫 글자 Nms · 첫 문장 Nms · 첫 음성 준비 Nms · 재생 시작 Nms · 문장 5개 · 끊김 0회",
];

fn assert_replay_record(records: &[Value], who: &str, sample: &str) {
    assert_eq!(records.len(), 1);
    let record = records[0].as_object().unwrap();
    let keys: Vec<&str> = record.keys().map(String::as_str).collect();
    assert_eq!(
        keys,
        [
            "at", "characterId", "mode", "label", "firstTextMs", "firstSentenceMs", "firstAudioReadyMs", "firstPlayMs",
            "totalMs", "sentences", "underruns",
        ]
    );
    assert_eq!((record["characterId"].as_str(), record["mode"].as_str()), (Some(who), Some("replay")));
    assert_eq!(record["label"], sample);
    assert_eq!((&record["sentences"], &record["underruns"]), (&json!(5), &json!(0)));
    assert!(record["at"].as_str().is_some_and(|at| at.len() == 24 && at.ends_with('Z')));
    for key in ["firstTextMs", "firstSentenceMs", "firstAudioReadyMs", "firstPlayMs", "totalMs"] {
        assert!(record[key].is_number(), "{key}");
    }
}

#[test]
fn replaying_with_a_reference_voice_sends_node_shaped_worker_requests() {
    let (requests, records, log, sample) = replay("right-woman");
    let line = |id: &str, text: &str, reference: &str, reference_text: &str| {
        format!(
            r#"{{"id":"{id}","text":"{text}","refAudio":"<home>/refs/{reference}.wav","refText":"{reference_text}","rate":1,"out":"<audio>/<pid>-{id}.wav"}}"#
        )
    };
    assert_eq!(
        requests,
        [
            line("s1", "준비됐습니다.", "base", "기준 문장"),
            line("s2", "보고", "base", "기준 문장"),
            line("s3", "힘든 하루였어요.", "sad", "슬픈 문장"),
            line("s4", "정말 고마워요!", "sad", "슬픈 문장"),
            line("s5", "다음에 또 봐요?", "base", "기준 문장"),
            line("s6", "수정 파일은 c.swift 입니다.", "base", "기준 문장"),
        ]
    );
    assert_replay_record(&records, "right-woman", &sample);
    assert_eq!(log, REPLAY_LOG);
}

#[test]
fn replaying_with_a_named_voice_adds_tone_for_exclamations_and_questions() {
    let (requests, records, log, sample) = replay("left-man");
    let line = |id: &str, text: &str, instruct: &str| {
        format!(
            r#"{{"id":"{id}","text":"{text}","voice":"Sohee","instruct":"{instruct}","rate":1.1,"out":"<audio>/<pid>-{id}.wav"}}"#
        )
    };
    assert_eq!(
        requests,
        [
            line("s1", "준비됐습니다.", "차분하게"),
            line("s2", "보고", "차분하게"),
            line("s3", "힘든 하루였어요.", "차분하게"),
            line("s4", "정말 고마워요!", "차분하게, 밝게"),
            line("s5", "다음에 또 봐요?", "차분하게, 묻듯이"),
            line("s6", "수정 파일은 c.swift 입니다.", "차분하게"),
        ]
    );
    assert_replay_record(&records, "left-man", &sample);
    assert_eq!(log, REPLAY_LOG);
}

static READY: OnceCell<()> = OnceCell::const_new();

async fn test_pool() -> Option<PgPool> {
    let (Ok(url), Ok(migrations)) = (std::env::var("OFFICE_TEST_DATABASE_URL"), std::env::var("OFFICE_MIGRATIONS_DIR"))
    else {
        eprintln!("OFFICE_TEST_DATABASE_URL·OFFICE_MIGRATIONS_DIR이 없어 건너뜀");
        return None;
    };
    let pool = db::connect(&url).await.unwrap();
    READY
        .get_or_init(|| async {
            migrate::run(&pool, Path::new(&migrations)).await.unwrap();
        })
        .await;
    Some(pool)
}

#[tokio::test]
async fn the_voice_query_returns_the_newest_turn_and_latest_assistant_reply_of_the_character() {
    let Some(pool) = test_pool().await else { return };
    let mut transaction = pool.begin().await.unwrap();
    // 시험 전용 임시 테이블로 실제 테이블을 가린다. 실사용 행 변경 없음.
    sqlx::raw_sql(
        "CREATE TEMP TABLE cli_sessions(id text, character_id text) ON COMMIT DROP;
         CREATE TEMP TABLE turns(id text, cli_session_id text, status text, started_at timestamptz) ON COMMIT DROP;
         CREATE TEMP TABLE messages(turn_id text, role text, text text, received_at timestamptz) ON COMMIT DROP;
         INSERT INTO cli_sessions VALUES ('sa','a'),('sb','b');
         INSERT INTO turns VALUES ('old','sa','completed','2026-01-01'),
           ('new','sa','running','2026-01-03'),('other','sb','running','2026-01-04');
         INSERT INTO messages VALUES ('new','assistant','이전','2026-01-03'),
           ('new','assistant','최신 답변','2026-01-04'),('new','user','사용자 내용','2026-01-05');",
    )
    .execute(&mut *transaction)
    .await
    .unwrap();
    let turn = read_voice_turn(&mut *transaction, "a", "2026-01-02").await.unwrap().unwrap();
    assert_eq!(turn["id"], "new");
    assert_eq!(turn["response"], "최신 답변");
    assert_eq!(turn["characterId"], "a");
    let keys: Vec<&str> = turn.as_object().unwrap().keys().map(String::as_str).collect();
    assert_eq!(keys, ["id", "characterId", "status", "startedAt", "response"]);
    assert!(turn["startedAt"].as_str().is_some_and(|at| at.len() == 24 && at.starts_with("2026-01-0") && at.ends_with('Z')));
    assert_eq!(read_voice_turn(&mut *transaction, "a", "2026-01-06").await.unwrap(), None);
    transaction.rollback().await.unwrap();
}

fn dead_pool() -> PgPool {
    db::connect_lazy("postgres://officestra@127.0.0.1:9/unused", Duration::from_millis(100)).unwrap()
}

fn app_state(pool: PgPool, voice_follow: Arc<VoiceFollowControl>) -> AppState {
    AppState {
        pool,
        runtime: Arc::new(Runtime::new(None, None)),
        release_id: None,
        cost_summary: Arc::default(),
        realtime: Arc::default(),
        embedding: LocalEmbeddingService::default(),
        model_catalog: None,
        cli_updates: Arc::default(),
        usage_summary: None,
        roleplay: None,
        voice_follow,
    }
}

async fn call(state: &AppState, method: &str, path: &str, body: Option<&str>) -> (StatusCode, Value) {
    let mut request = Request::builder().method(method).uri(path);
    if body.is_some() {
        request = request.header(header::CONTENT_TYPE, "application/json");
    }
    let request = request.body(body.map_or_else(Body::empty, |body| Body::from(body.to_string()))).unwrap();
    let response = router(state.clone()).oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

struct RecordingHandle(Arc<Mutex<Vec<String>>>);

impl FollowerHandle for RecordingHandle {
    fn kill(&mut self) {
        self.0.lock().unwrap().push("SIGTERM".into());
    }
}

#[tokio::test]
async fn voice_routes_validate_requests_like_node() {
    let calls: Arc<Mutex<Vec<String>>> = Arc::default();
    let recorded = Arc::clone(&calls);
    let spawn: SpawnFollower = Arc::new(move |character_id, _events| {
        recorded.lock().unwrap().push(format!("spawn {character_id}"));
        Ok(Box::new(RecordingHandle(Arc::clone(&recorded))) as Box<dyn FollowerHandle>)
    });
    let state = app_state(dead_pool(), Arc::new(VoiceFollowControl::new(spawn)));
    let invalid = json!({ "error": "characterId와 유효한 since가 필요합니다." });
    for path in [
        "/api/voice-feed",
        "/api/voice-feed?since=2026-01-01",
        "/api/voice-feed?characterId=right-woman",
        "/api/voice-feed?characterId=right-woman&since=",
        "/api/voice-feed?characterId=../x&since=2026-01-01",
        "/api/voice-feed?characterId=right-woman&since=nope",
    ] {
        assert_eq!(call(&state, "GET", path, None).await, (StatusCode::BAD_REQUEST, invalid.clone()), "{path}");
    }
    let stopped = json!({ "characterId": null, "state": "stopped", "error": null });
    assert_eq!(call(&state, "GET", "/api/voice-follow", None).await, (StatusCode::OK, stopped.clone()));
    assert_eq!(call(&state, "POST", "/api/voice-follow", None).await.0, StatusCode::UNSUPPORTED_MEDIA_TYPE);
    let enabled_error = json!({ "error": "enabled must be a boolean" });
    assert_eq!(call(&state, "POST", "/api/voice-follow", Some("")).await, (StatusCode::BAD_REQUEST, enabled_error.clone()));
    assert_eq!(
        call(&state, "POST", "/api/voice-follow", Some(r#"{"enabled":"yes"}"#)).await,
        (StatusCode::BAD_REQUEST, enabled_error)
    );
    assert_eq!(
        call(&state, "POST", "/api/voice-follow", Some("null")).await,
        (StatusCode::BAD_REQUEST, json!({ "error": "Cannot read properties of null (reading 'enabled')" }))
    );
    assert_eq!(
        call(&state, "POST", "/api/voice-follow", Some(r#"{"enabled":true,"characterId":"a b"}"#)).await,
        (StatusCode::BAD_REQUEST, json!({ "error": "characterId is required" }))
    );
    let starting = json!({ "characterId": "right-woman", "state": "starting", "error": null });
    assert_eq!(
        call(&state, "POST", "/api/voice-follow", Some(r#"{"enabled":true,"characterId":"right-woman"}"#)).await,
        (StatusCode::OK, starting.clone())
    );
    assert_eq!(call(&state, "GET", "/api/voice-follow", None).await, (StatusCode::OK, starting));
    assert_eq!(call(&state, "POST", "/api/voice-follow", Some(r#"{"enabled":false}"#)).await, (StatusCode::OK, stopped));
    assert_eq!(*calls.lock().unwrap(), ["spawn right-woman", "SIGTERM"]);
}

#[tokio::test]
async fn the_voice_feed_route_reads_the_database_with_a_normalised_since() {
    let Some(pool) = test_pool().await else { return };
    let state = app_state(pool, Arc::default());
    let path = format!("/api/voice-feed?characterId=voice-route-{}&since=2026-01-02", std::process::id());
    assert_eq!(call(&state, "GET", &path, None).await, (StatusCode::OK, json!({ "turn": null })));
}
