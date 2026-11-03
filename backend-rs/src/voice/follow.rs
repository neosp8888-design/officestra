// 직원 응답이 스트리밍되는 동안 완성된 문장부터 음성으로 합성해 바로 재생하고, 첫 음성까지의 지연을 기록한다.
// Node voice/voice-follow.mjs를 옮긴 따라 읽기 실행 파일의 본체다. 백엔드가 띄우면 유닉스 소켓으로 준비를 알리고, 소켓이 닫히면 멈춘다.

use std::collections::{HashMap, VecDeque};
use std::fs;
use std::io::{self, Write};
use std::os::fd::FromRawFd;
use std::path::Path;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Map, Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::signal::unix::{Signal, SignalKind, signal};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
use tokio::time::{Instant, sleep_until};

use super::feed::{FeedEvent, FeedOptions, FeedSubscription, subscribe_voice_feed};
use super::follow_lock::acquire_follow_lock;
use super::speakable::{PARAGRAPH_BREAK, SentenceStream, parse_directions};
use super::{
    CHANNEL_FD_ENV, DEFAULT_VOICES, MISSING_PYTHON_EXIT, MODEL_START_FAILURE_EXIT, WORKER_SOURCE, FOLLOW_BUSY_EXIT, fs_message,
    voice_home_directory, voice_python_path,
};
use crate::child_process::spawn_error_message;
use crate::js::{
    date_parse, iso_string, join_path, js_join, js_string, js_to_number, js_truthy, node_tmpdir, now_milliseconds, parse_js_json,
    resolve_path, string_to_number, to_js_json, uv_error,
};
use crate::roleplay::js_regex::JsRegExp;

/// 명령줄 값. 값 없이 쓴 옵션은 JS의 true다.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Argument {
    Text(String),
    Flag,
}

impl Argument {
    /// 템플릿 문자열·URL에 넣을 때. true는 "true"다.
    fn text(&self) -> String {
        match self {
            Self::Text(text) => text.clone(),
            Self::Flag => "true".into(),
        }
    }

    /// Number(값). true는 1이다.
    fn number(&self) -> f64 {
        match self {
            Self::Text(text) => string_to_number(text),
            Self::Flag => 1.0,
        }
    }
}

/// parseArgs. `--키 값`이고 다음 항목이 비었거나 `--`로 시작하면 값 없는 옵션이다.
fn parse_args(list: &[String]) -> HashMap<String, Argument> {
    let mut out = HashMap::new();
    let mut index = 0;
    while index < list.len() {
        let key = list[index].strip_prefix("--").unwrap_or(&list[index]).to_string();
        match list.get(index + 1) {
            Some(next) if !next.is_empty() && !next.starts_with("--") => {
                out.insert(key, Argument::Text(next.clone()));
                index += 1;
            }
            _ => {
                out.insert(key, Argument::Flag);
            }
        }
        index += 1;
    }
    out
}

/// `new Date().toTimeString().slice(0, 8)`. 현지 시각 HH:MM:SS다.
fn clock() -> String {
    let seconds = (now_milliseconds() / 1_000.0).floor() as libc::time_t;
    // SAFETY: localtime_r는 넘긴 두 포인터만 읽고 쓰며 tm은 0으로 채운 값에서 시작한다.
    let mut parts: libc::tm = unsafe { std::mem::zeroed() };
    unsafe { libc::localtime_r(&seconds, &mut parts) };
    format!("{:02}:{:02}:{:02}", parts.tm_hour, parts.tm_min, parts.tm_sec)
}

/// console.log(`[HH:MM:SS] 메시지`). 표준출력이 닫혀도 멈추지 않는다.
fn log(message: &str) {
    let _ = writeln!(io::stdout(), "[{}] {message}", clock());
}

/// 처리되지 않은 오류로 끝날 때. Node는 스택과 함께 종료 코드 1로 끝난다.
fn fatal(message: &str) -> i32 {
    let _ = writeln!(io::stderr(), "{message}");
    1
}

/// JS 템플릿 문자열 `${값}`. 없는 값은 "undefined"다.
fn template(value: Option<&Value>) -> String {
    value.map_or_else(|| "undefined".into(), js_string)
}

/// `값 ?? '-'`.
fn or_dash(value: Option<&Value>) -> String {
    match value {
        None | Some(Value::Null) => "-".into(),
        Some(value) => js_string(value),
    }
}

/// 객체의 속성. 객체가 아니면 undefined다.
fn field<'a>(value: &'a Value, key: &str) -> Option<&'a Value> {
    value.as_object().and_then(|fields| fields.get(key))
}

/// rm(path, { force: true }). 결과를 기다리지 않는 Node와 같이 실패는 무시한다.
fn remove_quietly(path: &str) {
    let _ = fs::remove_file(path);
}

/// Math.round. 0.5는 위로 올린다.
fn round(milliseconds: f64) -> Value {
    Value::from((milliseconds + 0.5).floor() as i64)
}

/// setTimeout 지연. 1~2147483647 밖이나 NaN이면 1ms다.
fn timeout_delay(milliseconds: f64) -> Duration {
    let after = if (1.0..=2_147_483_647.0).contains(&milliseconds) { milliseconds.trunc() } else { 1.0 };
    Duration::from_millis(after as u64)
}

/// mkdtemp(prefix). 프로세스마다 다른 폴더라 수동 시제품과 앱이 함께 돌아도 파일 이름이 겹치지 않는다. 끝나면 지운다.
struct TemporaryDirectory(String);

impl TemporaryDirectory {
    fn create(prefix: &str) -> Result<Self, String> {
        let template = format!("{prefix}XXXXXX");
        let mut bytes = template.clone().into_bytes();
        bytes.push(0);
        // SAFETY: NUL로 끝나는 쓰기 가능한 버퍼를 넘기고, mkdtemp는 그 안의 XXXXXX만 바꾼다.
        if unsafe { libc::mkdtemp(bytes.as_mut_ptr().cast()) }.is_null() {
            return Err(fs_message(&io::Error::last_os_error(), "mkdtemp", &template));
        }
        bytes.pop();
        Ok(Self(String::from_utf8_lossy(&bytes).into_owned()))
    }
}

impl Drop for TemporaryDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// 백엔드가 넘긴 소켓(Node process.send·'disconnect' 자리). 합성 작업자·afplay에는 넘기지 않는다.
fn open_channel() -> Option<UnixStream> {
    let fd: i32 = std::env::var(CHANNEL_FD_ENV).ok()?.trim().parse().ok()?;
    // SAFETY: fstat은 넘긴 버퍼에만 쓴다. 소켓일 때만 이 번호를 넘겨받는다.
    let mut status: libc::stat = unsafe { std::mem::zeroed() };
    if fd < 0 || unsafe { libc::fstat(fd, &mut status) } != 0 || (status.st_mode & libc::S_IFMT) != libc::S_IFSOCK {
        return None;
    }
    // SAFETY: 부모가 이 프로세스에만 넘긴 소켓이다. 자식 프로세스로 새지 않게 CLOEXEC를 건다.
    unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) };
    let stream = unsafe { std::os::unix::net::UnixStream::from_raw_fd(fd) };
    stream.set_nonblocking(true).ok()?;
    UnixStream::from_std(stream).ok()
}

/// 합성 작업자 사건. 표준출력 줄을 모두 넘긴 뒤 종료를 알린다.
enum WorkerEvent {
    Line(Value),
    SpawnError(String),
    Exit(Option<i32>),
}

/// 거둬들이기 전까지만 번호가 있는 자식. 끝난 프로세스 번호에 신호를 보내지 않는다.
#[derive(Clone, Default)]
struct ChildSlot(Arc<Mutex<Option<i32>>>);

impl ChildSlot {
    fn set(&self, pid: Option<u32>) {
        *self.0.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = pid.and_then(|pid| i32::try_from(pid).ok());
    }

    fn terminate(&self) {
        if let Some(pid) = *self.0.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) {
            // SAFETY: 아직 거두지 않은 자식에게 종료 신호만 보낸다.
            unsafe { libc::kill(pid, libc::SIGTERM) };
        }
    }
}

/// 상주 합성 작업자. 문장 하나씩 보내고 같은 id의 답을 기다린다.
struct Worker {
    stdin: UnboundedSender<String>,
    child: ChildSlot,
    counter: u64,
}

/// 합성 요청 주인. 바뀐 세션의 늦은 답은 파일만 지운다.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Owner {
    Warmup,
    Session(u64),
}

enum Event {
    Stop,
    Worker(WorkerEvent),
    Feed(FeedEvent),
    Play,
    Tick,
}

/// 따라 읽기 실행 한 번의 상태. 세션 밖의 모든 것(설정·작업자·기록 대기열)을 둔다.
struct Context {
    character_id: String,
    replay: bool,
    dry_run: bool,
    no_play: bool,
    rate: Option<Argument>,
    voices: Value,
    profile: Value,
    voice_directory: String,
    audio_directory: String,
    log_directory: String,
    letters: JsRegExp,
    worker: Option<Worker>,
    worker_events: Option<UnboundedReceiver<WorkerEvent>>,
    outstanding: HashMap<String, Owner>,
    ready: Option<Value>,
    warmup: Option<Value>,
    stopping: bool,
    generation: u64,
    records: Vec<(Option<Value>, Map<String, Value>)>,
    fatal: Option<String>,
    interrupt: Signal,
    terminate: Signal,
    channel: Option<UnixStream>,
}

impl Context {
    /// voiceFor. 기준 음성을 복제하는 직원은 괄호 지침의 말투에 맞는 기준 음성을 고른다.
    /// style_key는 JS `styles?.[style]`의 키다(말투가 없으면 "null", 예열은 "undefined").
    fn voice_for(&self, sentence: &str, style_key: &str) -> Map<String, Value> {
        let rate = match &self.rate {
            Some(argument) => argument.number(),
            None => match field(&self.profile, "rate") {
                None | Some(Value::Null) => 1.0,
                Some(rate) => js_to_number(rate),
            },
        };
        let rate = serde_json::Number::from_f64(rate).map_or(Value::Null, Value::Number);
        let reference = field(&self.profile, "styles")
            .and_then(|styles| field(styles, style_key))
            .filter(|reference| !reference.is_null())
            .unwrap_or(&self.profile);
        let mut settings = Map::new();
        if let Some(audio) = field(reference, "refAudio").filter(|audio| js_truthy(Some(audio))) {
            settings.insert("refAudio".into(), Value::String(join_path(&[&self.voice_directory, &js_string(audio)])));
            if let Some(text) = field(reference, "refText") {
                settings.insert("refText".into(), text.clone());
            }
            settings.insert("rate".into(), rate);
            return settings;
        }
        let emotion = field(&self.voices, "emotion");
        let tone = if sentence.ends_with('!') {
            emotion.and_then(|emotion| field(emotion, "exclaim"))
        } else if sentence.ends_with('?') {
            emotion.and_then(|emotion| field(emotion, "question"))
        } else {
            None
        };
        let parts: Vec<Value> =
            [field(&self.profile, "instruct"), tone].into_iter().flatten().filter(|part| js_truthy(Some(part))).cloned().collect();
        if let Some(voice) = field(&self.profile, "voice") {
            settings.insert("voice".into(), voice.clone());
        }
        settings.insert("instruct".into(), Value::String(js_join(&parts, ", ")));
        settings.insert("rate".into(), rate);
        settings
    }

    /// worker.synthesize. 요청 줄은 `{id, text, ...설정, out}` 순서다.
    fn synthesize(&mut self, text: &str, settings: Map<String, Value>, owner: Owner) {
        let Some(worker) = self.worker.as_mut() else { return };
        worker.counter += 1;
        let id = format!("s{}", worker.counter);
        let out = join_path(&[&self.audio_directory, &format!("{}-{id}.wav", std::process::id())]);
        let mut request = Map::new();
        request.insert("id".into(), Value::String(id.clone()));
        request.insert("text".into(), Value::String(text.into()));
        request.extend(settings);
        request.insert("out".into(), Value::String(out));
        let line = String::from_utf8(to_js_json(&Value::Object(request)).unwrap_or_default()).unwrap_or_default();
        let _ = worker.stdin.send(format!("{line}\n"));
        self.outstanding.insert(id, owner);
    }

    fn kill_worker(&self) {
        if let Some(worker) = &self.worker {
            worker.child.terminate();
        }
    }

    fn has_letter(&mut self, text: &str) -> bool {
        self.letters.test(text)
    }

    fn next_generation(&mut self) -> u64 {
        self.generation += 1;
        self.generation
    }

    fn settle_ready(&mut self, message: Value) {
        if self.ready.is_none() {
            self.ready = Some(message);
        }
    }

    /// 작업자 표준출력 한 줄·종료. 준비 뒤에 예기치 않게 끝나면 종료 코드 1로 멈춘다.
    fn handle_worker(&mut self, event: WorkerEvent, current: &mut Option<Current>) -> Result<(), i32> {
        match event {
            WorkerEvent::Line(message) => {
                if message.as_object().is_some_and(|fields| fields.contains_key("ready")) {
                    self.settle_ready(message);
                    return Ok(());
                }
                let Some(owner) = message.get("id").and_then(Value::as_str).and_then(|id| self.outstanding.remove(id)) else {
                    return Ok(());
                };
                match (owner, current.as_mut()) {
                    (Owner::Warmup, _) => self.warmup = Some(message),
                    (Owner::Session(generation), Some(current)) if current.session.generation == generation => {
                        current.session.on_synth_reply(&message, self);
                    }
                    // 취소된 세션의 답이다.
                    (Owner::Session(_), _) => {
                        if let Some(path) = field(&message, "path").filter(|path| js_truthy(Some(path))) {
                            remove_quietly(&js_string(path));
                        }
                    }
                }
                Ok(())
            }
            WorkerEvent::SpawnError(message) => {
                self.settle_ready(json!({ "ready": false, "error": message }));
                Ok(())
            }
            WorkerEvent::Exit(code) => {
                let was_ready = self.ready.is_some();
                let code = code.map_or_else(|| "null".to_string(), |code| code.to_string());
                if !was_ready {
                    self.settle_ready(json!({ "ready": false, "error": format!("합성 작업자 종료 (코드 {code})") }));
                }
                if self.stopping || !was_ready {
                    return Ok(());
                }
                log(&format!("합성 작업자가 예기치 않게 종료됨 (코드 {code})"));
                Err(1)
            }
        }
    }

    /// done.then(record). 지연 기록을 latency.jsonl에 덧붙이고 한 줄로 알린다. 쓰기 실패는 종료 코드 1이다.
    fn flush_records(&mut self) -> Result<(), i32> {
        for (label, marks) in std::mem::take(&mut self.records) {
            let mut line = Map::new();
            line.insert("at".into(), Value::String(iso_string(now_milliseconds())));
            line.insert("characterId".into(), Value::String(self.character_id.clone()));
            line.insert("mode".into(), Value::String(if self.replay { "replay" } else { "follow" }.into()));
            if let Some(label) = label {
                line.insert("label".into(), label);
            }
            line.extend(marks.clone());
            let text = String::from_utf8(to_js_json(&Value::Object(line)).unwrap_or_default()).unwrap_or_default();
            let path = join_path(&[&self.log_directory, "latency.jsonl"]);
            let appended = fs::OpenOptions::new()
                .append(true)
                .create(true)
                .open(&path)
                .and_then(|mut file| file.write_all(format!("{text}\n").as_bytes()));
            if let Err(error) = appended {
                return Err(fatal(&fs_message(&error, "open", &path)));
            }
            log(&format!(
                "기록: 첫 글자 {}ms · 첫 문장 {}ms · 첫 음성 준비 {}ms · 재생 시작 {}ms · 문장 {}개 · 끊김 {}회",
                or_dash(marks.get("firstTextMs")),
                or_dash(marks.get("firstSentenceMs")),
                or_dash(marks.get("firstAudioReadyMs")),
                or_dash(marks.get("firstPlayMs")),
                or_dash(marks.get("sentences")),
                or_dash(marks.get("underruns")),
            ));
        }
        Ok(())
    }

    /// 신호·백엔드 연결 끊김·작업자·피드·재생·흘려보내기 타이머 중 먼저 온 사건.
    async fn next_event(
        &mut self,
        playing: Option<&mut Playing>,
        feed: &mut Option<UnboundedReceiver<FeedEvent>>,
        tick: Option<Instant>,
    ) -> Event {
        tokio::select! {
            _ = self.interrupt.recv() => Event::Stop,
            _ = self.terminate.recv() => Event::Stop,
            () = disconnected(&mut self.channel) => Event::Stop,
            event = receive(&mut self.worker_events) => Event::Worker(event),
            event = receive(feed) => Event::Feed(event),
            () = play_finished(playing) => Event::Play,
            () = wait_until(tick) => Event::Tick,
        }
    }
}

/// 닫힌 수신기는 비우고 영영 기다린다.
async fn receive<T>(receiver: &mut Option<UnboundedReceiver<T>>) -> T {
    let closed = match receiver {
        Some(receiver) => match receiver.recv().await {
            Some(value) => return value,
            None => true,
        },
        None => false,
    };
    if closed {
        *receiver = None;
    }
    std::future::pending().await
}

/// 백엔드 쪽 소켓이 닫히면(백엔드 종료) 돌아온다. 소켓이 없으면 영영 기다린다.
async fn disconnected(channel: &mut Option<UnixStream>) {
    let Some(stream) = channel else { return std::future::pending().await };
    let mut buffer = [0_u8; 256];
    loop {
        match stream.read(&mut buffer).await {
            Ok(0) | Err(_) => return,
            Ok(_) => {}
        }
    }
}

async fn wait_until(deadline: Option<Instant>) {
    match deadline {
        Some(deadline) => sleep_until(deadline).await,
        None => std::future::pending().await,
    }
}

async fn play_finished(playing: Option<&mut Playing>) {
    match playing {
        Some(Playing::Pause { until, .. } | Playing::Wait { until, .. }) => sleep_until(*until).await,
        Some(Playing::Afplay { child, .. }) => {
            let _ = child.wait().await;
        }
        None => std::future::pending().await,
    }
}

/// startWorker. 합성 작업자 파일을 임시 폴더에 꺼내 음성 파이썬으로 띄운다. stdout은 JSON 응답 전용, stderr는 그대로 follow.log로 간다.
fn start_worker(python: &str, model: Option<&Value>, voice_directory: &str, audio_directory: &str) -> Result<(Worker, UnboundedReceiver<WorkerEvent>), i32> {
    let script = join_path(&[audio_directory, "tts_worker.py"]);
    if let Err(error) = fs::write(&script, WORKER_SOURCE) {
        return Err(fatal(&fs_message(&error, "open", &script)));
    }
    let (events, receiver) = unbounded_channel();
    let (stdin_sender, mut stdin_receiver) = unbounded_channel::<String>();
    let child = ChildSlot::default();
    let mut command = tokio::process::Command::new(python);
    command
        .arg(&script)
        .current_dir(voice_directory)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .env_remove(CHANNEL_FD_ENV);
    if let Some(model) = model.filter(|model| js_truthy(Some(model))) {
        command.env("LOCAL_VOICE_MODEL", js_string(model));
    }
    let mut process = match command.spawn() {
        Ok(process) => process,
        Err(error) => {
            let message = spawn_error_message(python, &error);
            // Node는 이 다섯 오류만 'error' 사건(준비 실패)으로 받고, 나머지는 처리되지 않은 오류로 끝난다.
            if !matches!(uv_error(&error).0, "EACCES" | "EAGAIN" | "EMFILE" | "ENFILE" | "ENOENT") {
                return Err(fatal(&format!("Error: {message}")));
            }
            let _ = events.send(WorkerEvent::SpawnError(message));
            return Ok((Worker { stdin: stdin_sender, child, counter: 0 }, receiver));
        }
    };
    child.set(process.id());
    if let Some(mut stdin) = process.stdin.take() {
        tokio::spawn(async move {
            while let Some(line) = stdin_receiver.recv().await {
                if stdin.write_all(line.as_bytes()).await.is_err() {
                    break;
                }
            }
        });
    }
    let stdout = process.stdout.take();
    let reaped = child.clone();
    tokio::spawn(async move {
        if let Some(stdout) = stdout {
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                if let Ok(message) = parse_js_json(&line) {
                    let _ = events.send(WorkerEvent::Line(message));
                }
            }
        }
        let status = process.wait().await;
        reaped.set(None);
        let _ = events.send(WorkerEvent::Exit(status.ok().and_then(|status| status.code())));
    });
    Ok((Worker { stdin: stdin_sender, child, counter: 0 }, receiver))
}

/// 재생 대기열 항목. 합성 답의 path·audioMs와 문장 앞 쉼이다.
struct PlayItem {
    path: Option<String>,
    audio_ms: f64,
    pause_ms: u64,
}

/// 지금 재생 중인 것. 앞 쉼 타이머, 소리 없이 음성 길이만큼 기다리기(--no-play), afplay 중 하나다.
enum Playing {
    Pause { until: Instant, item: PlayItem },
    Wait { until: Instant, path: String },
    Afplay { child: tokio::process::Child, path: String },
}

/// 합성 답을 기다리는 문장. 답이 오면 이어서 처리한다(Node의 await 뒤).
struct Waiting {
    sentence: String,
    style: Option<&'static str>,
    pause_ms: u64,
}

/// 한 응답(턴)을 읽는 세션. 합성과 재생을 겹쳐서 다음 문장을 재생 중에 미리 만든다.
struct Session {
    generation: u64,
    label: Option<Value>,
    started_at: Instant,
    stream: SentenceStream,
    synth_queue: VecDeque<String>,
    play_queue: VecDeque<PlayItem>,
    /// label·startedAt 뒤에 붙는 측정값. 처음 기록한 순서대로 남는다.
    marks: Map<String, Value>,
    cancelled: bool,
    synthesizing: bool,
    waiting: Option<Waiting>,
    playing: Option<Playing>,
    finished: bool,
    underruns: u64,
    spoken_sentences: u64,
    /// 괄호 지침의 말투는 새 지침이 나오거나 문단이 끝날 때까지 이어진다. 쉼은 지침이 붙은 문장에만 쓴다.
    mood: Option<&'static str>,
    pending_pause_ms: u64,
    done: bool,
}

impl Session {
    fn new(generation: u64, label: Option<Value>) -> Self {
        Self {
            generation,
            label,
            started_at: Instant::now(),
            stream: SentenceStream::new(true),
            synth_queue: VecDeque::new(),
            play_queue: VecDeque::new(),
            marks: Map::new(),
            cancelled: false,
            synthesizing: false,
            waiting: None,
            playing: None,
            finished: false,
            underruns: 0,
            spoken_sentences: 0,
            mood: None,
            pending_pause_ms: 0,
            done: false,
        }
    }

    fn elapsed(&self) -> f64 {
        self.started_at.elapsed().as_secs_f64() * 1_000.0
    }

    fn mark(&mut self, key: &str) {
        if self.marks.get(key).is_none_or(Value::is_null) {
            let value = round(self.elapsed());
            self.marks.insert(key.into(), value);
        }
    }

    fn pump_synth(&mut self, ctx: &mut Context) {
        loop {
            if self.synthesizing || self.cancelled {
                return;
            }
            let Some(raw) = self.synth_queue.pop_front() else {
                self.maybe_done(ctx);
                return;
            };
            if raw == PARAGRAPH_BREAK {
                self.mood = None;
                continue;
            }
            let parsed = parse_directions(&raw);
            if parsed.style.is_some() {
                self.mood = parsed.style;
            }
            let pause_ms = parsed.pause_ms.max(self.pending_pause_ms);
            // 지침만 있고 읽을 말이 없으면 쉼을 다음 문장에 넘긴다.
            if !ctx.has_letter(&parsed.text) {
                self.pending_pause_ms = pause_ms;
                continue;
            }
            self.pending_pause_ms = 0;
            let style = self.mood;
            if ctx.dry_run {
                self.after_synth(&json!({ "path": null, "synthMs": 0, "audioMs": 0 }), &parsed.text, style, pause_ms, ctx);
                continue;
            }
            self.synthesizing = true;
            let settings = ctx.voice_for(&parsed.text, style.unwrap_or("null"));
            ctx.synthesize(&parsed.text, settings, Owner::Session(self.generation));
            self.waiting = Some(Waiting { sentence: parsed.text, style, pause_ms });
            return;
        }
    }

    /// 합성 답이 왔을 때. 그사이 취소됐으면 만든 파일만 지운다.
    fn on_synth_reply(&mut self, result: &Value, ctx: &mut Context) {
        self.synthesizing = false;
        let Some(waiting) = self.waiting.take() else { return };
        if self.cancelled {
            if let Some(path) = field(result, "path").filter(|path| js_truthy(Some(path))) {
                remove_quietly(&js_string(path));
            }
            return;
        }
        self.after_synth(result, &waiting.sentence, waiting.style, waiting.pause_ms, ctx);
        self.pump_synth(ctx);
    }

    fn after_synth(&mut self, result: &Value, sentence: &str, style: Option<&'static str>, pause_ms: u64, ctx: &mut Context) {
        if js_truthy(field(result, "error")) {
            log(&format!("합성 실패: {}", template(field(result, "error"))));
            return;
        }
        self.mark("firstAudioReadyMs");
        let tags: Vec<String> =
            [style.map(str::to_string), (pause_ms > 0).then(|| format!("쉼 {pause_ms}ms"))].into_iter().flatten().collect();
        let tags = if tags.is_empty() { String::new() } else { format!(" [{}]", tags.join(" · ")) };
        log(&format!(
            "합성 {}ms → 음성 {}ms{tags} · {sentence}",
            template(field(result, "synthMs")),
            template(field(result, "audioMs")),
        ));
        let path = field(result, "path").filter(|path| js_truthy(Some(path))).map(js_string);
        let audio_ms = field(result, "audioMs").map_or(f64::NAN, js_to_number);
        self.play_queue.push_back(PlayItem { path, audio_ms, pause_ms });
        self.pump_play(ctx);
    }

    fn pump_play(&mut self, ctx: &mut Context) {
        loop {
            if self.playing.is_some() || self.cancelled {
                return;
            }
            let Some(item) = self.play_queue.pop_front() else {
                // 재생할 게 없는데 아직 만들 문장이 남아 있으면 끊김으로 센다.
                if self.spoken_sentences > 0 && (self.synthesizing || !self.synth_queue.is_empty()) {
                    self.underruns += 1;
                }
                self.maybe_done(ctx);
                return;
            };
            self.spoken_sentences += 1;
            self.mark("firstPlayMs");
            if ctx.dry_run || item.path.is_none() {
                continue;
            }
            // "잠시 침묵" 같은 지침이 붙은 문장은 앞에서 쉬었다가 읽는다.
            if item.pause_ms > 0 {
                let until = Instant::now() + timeout_delay(item.pause_ms as f64);
                self.playing = Some(Playing::Pause { until, item });
            } else {
                self.start(item, ctx);
            }
            return;
        }
    }

    fn start(&mut self, item: PlayItem, ctx: &mut Context) {
        let Some(path) = item.path else { return };
        if ctx.no_play {
            // 소리는 내지 않되 음성 길이만큼 기다려 실제 재생과 같은 끊김을 잰다.
            self.playing = Some(Playing::Wait { until: Instant::now() + timeout_delay(item.audio_ms), path });
            return;
        }
        let spawned = tokio::process::Command::new("afplay")
            .arg(&path)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .env_remove(CHANNEL_FD_ENV)
            .spawn();
        match spawned {
            Ok(child) => self.playing = Some(Playing::Afplay { child, path }),
            // Node는 afplay의 'error' 사건을 받지 않아 처리되지 않은 오류로 끝난다.
            Err(error) => ctx.fatal = Some(format!("Error: {}", spawn_error_message("afplay", &error))),
        }
    }

    /// 앞 쉼이 끝났거나 재생(기다리기·afplay)이 끝났을 때.
    fn on_play_finished(&mut self, ctx: &mut Context) {
        match self.playing.take() {
            Some(Playing::Pause { item, .. }) => self.start(item, ctx),
            Some(Playing::Wait { path, .. } | Playing::Afplay { path, .. }) => {
                remove_quietly(&path);
                self.pump_play(ctx);
            }
            None => {}
        }
    }

    fn maybe_done(&mut self, ctx: &mut Context) {
        if self.finished && !self.synthesizing && self.synth_queue.is_empty() && self.play_queue.is_empty() && self.playing.is_none() {
            let total = round(self.elapsed());
            self.marks.insert("totalMs".into(), total);
            self.marks.insert("sentences".into(), Value::from(self.spoken_sentences));
            self.marks.insert("underruns".into(), Value::from(self.underruns));
            self.resolve(ctx);
        }
    }

    /// resolveDone(marks). 처음 한 번만 기록 대기열에 넣는다.
    fn resolve(&mut self, ctx: &mut Context) {
        if !self.done {
            self.done = true;
            ctx.records.push((self.label.clone(), self.marks.clone()));
        }
    }

    fn enqueue(&mut self, sentences: Vec<String>, ctx: &mut Context) {
        if sentences.is_empty() {
            return;
        }
        if sentences.iter().any(|sentence| sentence != PARAGRAPH_BREAK) {
            self.mark("firstSentenceMs");
        }
        self.synth_queue.extend(sentences);
        self.pump_synth(ctx);
    }

    fn update(&mut self, text: &str, ctx: &mut Context) {
        if !text.is_empty() {
            self.mark("firstTextMs");
        }
        let sentences = self.stream.push(text);
        self.enqueue(sentences, ctx);
    }

    fn finish(&mut self, ctx: &mut Context) {
        let sentences = self.stream.finish();
        self.enqueue(sentences, ctx);
        self.finished = true;
        self.maybe_done(ctx);
    }

    fn cancel(&mut self, ctx: &mut Context) {
        self.cancelled = true;
        for item in self.play_queue.drain(..) {
            if let Some(path) = item.path {
                remove_quietly(&path);
            }
        }
        self.synth_queue.clear();
        // 기다리기 타이머는 clearTimeout처럼 그냥 버린다. 그 파일은 임시 폴더와 함께 지워진다.
        if let Some(Playing::Afplay { mut child, path }) = self.playing.take() {
            if let Some(pid) = child.id().and_then(|pid| i32::try_from(pid).ok()) {
                // SAFETY: 아직 거두지 않은 afplay에 종료 신호만 보낸다.
                unsafe { libc::kill(pid, libc::SIGTERM) };
            }
            // Node 'exit' 처리처럼 afplay가 끝나면 파일을 지운다.
            tokio::spawn(async move {
                let _ = child.wait().await;
                remove_quietly(&path);
            });
        }
        self.marks.insert("cancelled".into(), Value::Bool(true));
        self.resolve(ctx);
    }
}

/// 지금 읽는 턴. 흘려보내기 시험은 id 없이 세션 하나만 쓴다.
struct Current {
    id: Option<Value>,
    started: Option<f64>,
    session: Session,
    finished: bool,
}

/// 파일을 초당 글자 수만큼 흘려 LLM 스트리밍을 흉내 낸다. 4090 없이 파이프라인을 확인할 때 쓴다.
struct Replay {
    units: Vec<u16>,
    cps: f64,
    started: Instant,
    tick: Option<Instant>,
}

impl Replay {
    const STEP: Duration = Duration::from_millis(50);

    fn step(&mut self, session: &mut Session, ctx: &mut Context) {
        let length = self.units.len() as f64;
        let shown = crate::js::js_min(length, (self.started.elapsed().as_secs_f64() * self.cps).floor());
        session.update(&String::from_utf16_lossy(&self.units[..slice_end(shown, self.units.len())]), ctx);
        if shown < length {
            self.tick = Some(Instant::now() + Self::STEP);
        } else {
            self.tick = None;
            session.finish(ctx);
        }
    }
}

/// text.slice(0, end)의 끝 위치. NaN은 0, 음수는 끝에서 센다.
fn slice_end(end: f64, length: usize) -> usize {
    if end.is_nan() {
        return 0;
    }
    let length = length as f64;
    let end = if end < 0.0 { (length + end.trunc()).max(0.0) } else { end.trunc().min(length) };
    end as usize
}

/// 변경 알림으로 받은 대상 직원의 답변. 새 턴이면 읽던 것을 끊고 새로 읽는다.
fn on_turn(turn: &Value, launched_at: f64, current: &mut Option<Current>, ctx: &mut Context) {
    let started = field(turn, "startedAt").map(js_string).and_then(|text| date_parse(&text));
    if started.is_some_and(|started| started < launched_at - 2_000.0) {
        return;
    }
    let id = field(turn, "id");
    if id != current.as_ref().and_then(|current| current.id.as_ref()) {
        // 지연된 이전 턴이나 삭제로 되돌아간 턴을 다시 읽지 않는다.
        if current.as_ref().is_some_and(|current| matches!((started, current.started), (Some(new), Some(old)) if new < old)) {
            return;
        }
        if let Some(current) = current.as_mut() {
            current.session.cancel(ctx);
        }
        log(&format!("새 턴 {} ({})", template(id), template(field(turn, "status"))));
        let generation = ctx.next_generation();
        *current = Some(Current { id: id.cloned(), started, session: Session::new(generation, id.cloned()), finished: false });
    }
    let Some(current) = current.as_mut() else { return };
    if current.finished {
        return;
    }
    let response = match field(turn, "response") {
        None | Some(Value::Null) => String::new(),
        Some(response) => js_string(response),
    };
    current.session.update(&response, ctx);
    if !matches!(field(turn, "status").and_then(Value::as_str), Some("pending" | "running")) {
        current.finished = true;
        current.session.finish(ctx);
    }
}

/// stop(). 읽던 세션을 취소하고 작업자를 끝낸 뒤 종료 코드 0이다. Node는 취소 기록을 쓰기 전에 끝나므로 기록하지 않는다.
fn stop(ctx: &mut Context, current: &mut Option<Current>) -> i32 {
    ctx.stopping = true;
    if let Some(current) = current.as_mut() {
        current.session.cancel(ctx);
    }
    ctx.kill_worker();
    ctx.records.clear();
    0
}

/// 따라 읽기 실행 파일 진입점. 돌려준 값이 종료 코드다.
pub async fn run(arguments: Vec<String>) -> i32 {
    match run_follower(&arguments).await {
        Ok(()) => 0,
        Err(code) => code,
    }
}

/// Err는 바로 끝낼 종료 코드다. 임시 폴더·잠금은 지역 변수라 돌아가며 정리된다(Node process 'exit' 처리).
async fn run_follower(arguments: &[String]) -> Result<(), i32> {
    // 합성 작업자를 띄우기 전에 소켓에 CLOEXEC를 건다.
    let channel = open_channel();
    let (Ok(interrupt), Ok(terminate)) = (signal(SignalKind::interrupt()), signal(SignalKind::terminate())) else {
        return Err(fatal("Error: 종료 신호 처리기를 등록하지 못했습니다."));
    };
    let args = parse_args(arguments);
    let backend = match args.get("backend") {
        Some(backend) => backend.text(),
        None => {
            let port = std::env::var_os("OFFICE_BACKEND_PORT").map_or_else(|| "4317".into(), |port| port.to_string_lossy().into_owned());
            format!("http://127.0.0.1:{port}")
        }
    };
    let character_id = args.get("follow").map_or_else(|| "right-woman".to_string(), Argument::text);
    let voice_directory = resolve_path(&voice_home_directory());
    let local_voices = join_path(&[&voice_directory, "voices.json"]);
    // 개인 설정이 있으면 그것을, 없으면 실행 파일에 넣은 기본 설정을 쓴다.
    let voices_text = if Path::new(&local_voices).exists() {
        let bytes = fs::read(&local_voices).map_err(|error| fatal(&format!("Error: {}", fs_message(&error, "open", &local_voices))))?;
        String::from_utf8_lossy(&bytes).into_owned()
    } else {
        DEFAULT_VOICES.to_string()
    };
    let voices = parse_js_json(&voices_text).map_err(|error| fatal(&format!("SyntaxError: {error}")))?;
    let log_directory = join_path(&[&voice_directory, "logs"]);
    let audio = TemporaryDirectory::create(&join_path(&[&node_tmpdir(), "officestra-local-voice-"]))
        .map_err(|message| fatal(&format!("Error: {message}")))?;
    fs::create_dir_all(&log_directory)
        .map_err(|error| fatal(&format!("Error: {}", fs_message(&error, "mkdir", &log_directory))))?;
    let replay = args.get("replay").cloned();
    // 수동 시제품과 앱 제어기가 같은 응답을 동시에 읽지 않게 한다. 흘려보내기 시험은 잠그지 않는다.
    let _lock = match replay {
        Some(_) => None,
        None => Some(acquire_follow_lock(&voice_home_directory()).map_err(|message| {
            let _ = writeln!(io::stderr(), "{message}");
            FOLLOW_BUSY_EXIT
        })?),
    };
    let dry_run = args.contains_key("dry-run");
    let profile = voices
        .as_object()
        .and_then(|voices| voices.get(&character_id).filter(|profile| !profile.is_null()).or_else(|| voices.get("default")));
    let profile = match profile {
        Some(profile) if !profile.is_null() => profile.clone(),
        missing if !dry_run => {
            let shown = if missing.is_some() { "null" } else { "undefined" };
            return Err(fatal(&format!("TypeError: Cannot read properties of {shown} (reading 'model')")));
        }
        _ => Value::Null,
    };
    let (worker, worker_events) = if dry_run {
        (None, None)
    } else {
        let python = voice_python_path();
        if !Path::new(&python).exists() {
            log(&format!("음성 파이썬 환경이 없습니다: {python}"));
            return Err(MISSING_PYTHON_EXIT);
        }
        let (worker, events) = start_worker(&python, field(&profile, "model"), &voice_directory, &audio.0)?;
        (Some(worker), Some(events))
    };
    let mut ctx = Context {
        character_id: character_id.clone(),
        replay: replay.is_some(),
        dry_run,
        no_play: args.contains_key("no-play"),
        rate: args.get("rate").cloned(),
        voices,
        profile,
        voice_directory,
        audio_directory: audio.0.clone(),
        log_directory,
        letters: JsRegExp::new(r"[\p{L}\p{N}]", "u").map_err(|message| fatal(&message))?,
        worker,
        worker_events,
        outstanding: HashMap::new(),
        ready: None,
        warmup: None,
        stopping: false,
        generation: 0,
        records: Vec::new(),
        fatal: None,
        interrupt,
        terminate,
        channel,
    };
    let mut current: Option<Current> = None;
    let mut feed: Option<UnboundedReceiver<FeedEvent>> = None;
    if ctx.worker.is_some() {
        while ctx.ready.is_none() {
            match ctx.next_event(None, &mut feed, None).await {
                Event::Stop => return Err(stop(&mut ctx, &mut current)),
                Event::Worker(event) => ctx.handle_worker(event, &mut current)?,
                _ => {}
            }
        }
        let ready = ctx.ready.clone().unwrap_or(Value::Null);
        if !js_truthy(field(&ready, "ready")) {
            let reason = match field(&ready, "error") {
                None | Some(Value::Null) => "원인을 확인할 수 없습니다.".to_string(),
                Some(error) => js_string(error),
            };
            log(&format!("합성 초기화 실패: {reason}"));
            ctx.stopping = true;
            ctx.kill_worker();
            return Err(MODEL_START_FAILURE_EXIT);
        }
        let settings = ctx.voice_for("준비됐습니다.", "undefined");
        ctx.synthesize("준비됐습니다.", settings, Owner::Warmup);
        while ctx.warmup.is_none() {
            match ctx.next_event(None, &mut feed, None).await {
                Event::Stop => return Err(stop(&mut ctx, &mut current)),
                Event::Worker(event) => ctx.handle_worker(event, &mut current)?,
                _ => {}
            }
        }
        let warmup = ctx.warmup.take().unwrap_or(Value::Null);
        if js_truthy(field(&warmup, "error")) {
            log(&format!("예열 실패: {}", template(field(&warmup, "error"))));
            ctx.stopping = true;
            ctx.kill_worker();
            return Err(MODEL_START_FAILURE_EXIT);
        }
        if let Some(path) = field(&warmup, "path").filter(|path| js_truthy(Some(path))) {
            remove_quietly(&js_string(path));
        }
        log(&format!(
            "합성 준비 완료 · {} · 적재 {}ms · 예열 {}ms",
            template(field(&ready, "model")),
            template(field(&ready, "loadMs")),
            or_dash(field(&warmup, "synthMs")),
        ));
    }
    if let Some(channel) = ctx.channel.as_mut() {
        let _ = channel.write_all(b"{\"ready\":true}\n").await;
    }
    let mut replaying = None;
    let mut subscription: Option<FeedSubscription> = None;
    let launched_at = now_milliseconds();
    match replay {
        Some(Argument::Text(path)) => {
            let bytes = fs::read(&path).map_err(|error| fatal(&format!("Error: {}", fs_message(&error, "open", &path))))?;
            let units: Vec<u16> = String::from_utf8_lossy(&bytes).encode_utf16().collect();
            let cps = args.get("cps").map_or(30.0, Argument::number);
            let mut session = Session::new(ctx.next_generation(), Some(Value::String(path)));
            let tick = if units.is_empty() {
                session.finish(&mut ctx);
                None
            } else {
                Some(Instant::now() + Replay::STEP)
            };
            current = Some(Current { id: None, started: None, session, finished: false });
            replaying = Some(Replay { units, cps, started: Instant::now(), tick });
        }
        Some(Argument::Flag) => {
            return Err(fatal(
                "TypeError [ERR_INVALID_ARG_TYPE]: The \"path\" argument must be of type string or an instance of Buffer or URL. Received type boolean (true)",
            ));
        }
        None => {
            log(&format!("{character_id}의 새 응답을 기다립니다."));
            let (sender, receiver) = unbounded_channel();
            let options = FeedOptions::new(&backend, &character_id, &iso_string(launched_at - 2_000.0));
            subscription = Some(subscribe_voice_feed(options, sender).map_err(|message| fatal(&format!("TypeError: {message}")))?);
            feed = Some(receiver);
        }
    }
    loop {
        if let Some(message) = ctx.fatal.take() {
            return Err(fatal(&message));
        }
        ctx.flush_records()?;
        if replaying.is_some() && current.as_ref().is_some_and(|current| current.session.done) {
            ctx.stopping = true;
            ctx.kill_worker();
            return Ok(());
        }
        let tick = replaying.as_ref().and_then(|replay| replay.tick);
        let playing = current.as_mut().and_then(|current| current.session.playing.as_mut());
        match ctx.next_event(playing, &mut feed, tick).await {
            Event::Stop => {
                if let Some(subscription) = subscription.take() {
                    subscription.close();
                }
                return Err(stop(&mut ctx, &mut current));
            }
            Event::Worker(event) => ctx.handle_worker(event, &mut current)?,
            Event::Feed(FeedEvent::Error(message)) => log(&format!("음성 변경 알림/조회 실패: {message}")),
            Event::Feed(FeedEvent::Turn(turn)) => on_turn(&turn, launched_at, &mut current, &mut ctx),
            Event::Play => {
                if let Some(current) = current.as_mut() {
                    current.session.on_play_finished(&mut ctx);
                }
            }
            Event::Tick => {
                if let (Some(replay), Some(current)) = (replaying.as_mut(), current.as_mut()) {
                    replay.step(&mut current.session, &mut ctx);
                }
            }
        }
    }
}
