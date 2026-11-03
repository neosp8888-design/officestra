// 음성 따라 읽기 모듈의 뿌리. 경로 규칙(Node voice/voice-paths.mjs), 최신 답변 조회(voice-feed.mjs), 따라 읽기 자식 프로세스 켜고 끄기(voice-follow-control.mjs)를 둔다.

pub mod feed;
pub mod follow;
pub mod follow_lock;
pub mod speakable;
#[cfg(test)]
mod tests;

use std::fs::OpenOptions;
use std::io;
use std::os::fd::AsRawFd;
use std::process::Stdio;
use std::sync::{Arc, Mutex, MutexGuard, Weak};

use serde::Serialize;
use serde_json::Value;
use sqlx::{AssertSqlSafe, PgExecutor};
use tokio::io::{AsyncBufReadExt, BufReader};

use crate::child_process::spawn_error_message;
use crate::js::{join_path, js_trim, js_truthy, node_fs_message, resolve_path, uv_error};
use crate::turn_feed::iso;

/// 합성 작업자와 기본 목소리 설정. Node backend/src/voice의 같은 파일을 실행 파일에 넣는다.
pub const WORKER_SOURCE: &str = include_str!("../voice/tts_worker.py");
pub const DEFAULT_VOICES: &str = include_str!("../voice/voices.json");

/// 백엔드 실행 파일 옆에 두는 따라 읽기 실행 파일 이름.
pub const FOLLOWER_COMMAND: &str = "officestra-voice-follow";
/// 자식에게 넘긴 유닉스 소켓 번호를 알리는 환경 변수. Node fork의 NODE_CHANNEL_FD 자리다.
pub const CHANNEL_FD_ENV: &str = "OFFICE_VOICE_CHANNEL_FD";

/// 따라 읽기가 파이썬 환경을 찾지 못하면 이 코드로 끝난다.
pub const MISSING_PYTHON_EXIT: i32 = 2;
pub const FOLLOW_BUSY_EXIT: i32 = 3;
pub const MODEL_START_FAILURE_EXIT: i32 = 4;

fn environment_text(name: &str) -> String {
    std::env::var_os(name).map(|value| value.to_string_lossy().into_owned()).unwrap_or_default()
}

/// 음성 파이썬 환경과 기록 폴더 위치. 앱 번들 밖(~/.officestra/voice)에 둔다.
pub fn voice_home_directory() -> String {
    let configured = js_trim(&environment_text("OFFICE_VOICE_DIR")).to_string();
    if !configured.is_empty() {
        return configured;
    }
    resolve_path(&join_path(&[&crate::local_artifacts::home_directory(), ".officestra", "voice"]))
}

pub fn voice_python_path() -> String {
    let configured = js_trim(&environment_text("OFFICE_VOICE_PYTHON")).to_string();
    if !configured.is_empty() {
        return configured;
    }
    join_path(&[&voice_home_directory(), ".venv", "bin", "python"])
}

/// 직원 ID 모양 `/^[\w-]+$/`.
pub fn valid_character_id(id: &str) -> bool {
    !id.is_empty() && id.bytes().all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
}

/// 음성에는 최신 답변 하나만 필요하다. 작업 내역·비용·출처를 조립하지 않는다.
/// Node readVoiceTurn(client)처럼 풀이나 연결·트랜잭션을 받는다.
pub async fn read_voice_turn<'e, E: PgExecutor<'e>>(
    executor: E,
    character_id: &str,
    since: &str,
) -> Result<Option<Value>, sqlx::Error> {
    let sql = format!(
        r#"SELECT row_to_json(x) FROM (
    SELECT t.id, s.character_id AS "characterId", t.status,
      {started_at} AS "startedAt",
      COALESCE((SELECT m.text FROM messages m
        WHERE m.turn_id=t.id AND m.role='assistant'
        ORDER BY m.received_at DESC LIMIT 1), '') AS response
    FROM turns t JOIN cli_sessions s ON s.id=t.cli_session_id
    WHERE s.character_id=$1 AND t.started_at >= $2::timestamptz
    ORDER BY t.started_at DESC, t.id DESC LIMIT 1
  ) AS x"#,
        started_at = iso("t.started_at"),
    );
    sqlx::query_scalar(AssertSqlSafe(sql)).bind(character_id).bind(since).fetch_optional(executor).await
}

/// GET·POST /api/voice-follow 응답 `{characterId, state, error}`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FollowStatus {
    pub character_id: Option<String>,
    pub state: &'static str,
    pub error: Option<String>,
}

impl FollowStatus {
    fn stopped() -> Self {
        Self { character_id: None, state: "stopped", error: None }
    }
}

/// 자식 프로세스 사건. Node ChildProcess의 'message'·'error'·'exit'다. 신호로 끝나면 종료 코드가 없다.
#[derive(Clone, Debug)]
pub enum FollowerEvent {
    Message(Value),
    Error(String),
    Exit(Option<i32>),
}

pub type FollowerEvents = Arc<dyn Fn(FollowerEvent) + Send + Sync>;

/// 띄운 따라 읽기 자식. kill은 Node child.kill('SIGTERM')이다.
pub trait FollowerHandle: Send {
    fn kill(&mut self);
}

/// spawnVoiceFollower 자리. 사건은 이 함수가 돌아온 뒤에 보낸다(안에서 바로 부르지 않는다).
pub type SpawnFollower = Arc<dyn Fn(&str, FollowerEvents) -> Result<Box<dyn FollowerHandle>, String> + Send + Sync>;

struct Inner {
    child: Option<(u64, Box<dyn FollowerHandle>)>,
    generation: u64,
    status: FollowStatus,
}

/// 앱의 음성 지원 토글에 맞춰 직원 응답 따라 읽기 자식 프로세스를 켜고 끈다.
pub struct VoiceFollowControl {
    spawn_follower: SpawnFollower,
    inner: Arc<Mutex<Inner>>,
}

impl VoiceFollowControl {
    pub fn new(spawn_follower: SpawnFollower) -> Self {
        Self {
            spawn_follower,
            inner: Arc::new(Mutex::new(Inner { child: None, generation: 0, status: FollowStatus::stopped() })),
        }
    }

    /// 운영용. 백엔드 실행 파일 옆의 officestra-voice-follow를 띄운다.
    pub fn with_process_follower() -> Self {
        Self::new(process_spawner(default_follower_path(), None))
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    pub fn status(&self) -> FollowStatus {
        self.lock().status.clone()
    }

    /// Node set({enabled, characterId}). 오류 문구는 그대로 400 응답이 된다.
    pub fn set(&self, enabled: Option<&Value>, character_id: Option<&Value>) -> Result<FollowStatus, String> {
        let Some(Value::Bool(enabled)) = enabled else {
            return Err("enabled must be a boolean".into());
        };
        let mut inner = self.lock();
        if !enabled {
            stop_locked(&mut inner);
            return Ok(inner.status.clone());
        }
        let character_id = match character_id {
            Some(Value::String(id)) if valid_character_id(id) => id.clone(),
            _ => return Err("characterId is required".into()),
        };
        if inner.child.is_some() && inner.status.character_id.as_deref() == Some(character_id.as_str()) {
            return Ok(inner.status.clone());
        }
        stop_locked(&mut inner);
        inner.generation += 1;
        let generation = inner.generation;
        let events = child_events(Arc::downgrade(&self.inner), generation, character_id.clone());
        let child = (self.spawn_follower)(&character_id, events)?;
        inner.child = Some((generation, child));
        inner.status = FollowStatus { character_id: Some(character_id), state: "starting", error: None };
        Ok(inner.status.clone())
    }

    pub fn stop(&self) {
        stop_locked(&mut self.lock());
    }
}

impl Default for VoiceFollowControl {
    fn default() -> Self {
        Self::with_process_follower()
    }
}

fn stop_locked(inner: &mut Inner) {
    let child = inner.child.take();
    inner.status = FollowStatus::stopped();
    if let Some((_, mut child)) = child {
        child.kill();
    }
}

/// 지금 자식의 사건만 상태에 반영한다. 멈추거나 바뀐 자식의 늦은 사건은 버린다.
fn child_events(inner: Weak<Mutex<Inner>>, generation: u64, character_id: String) -> FollowerEvents {
    Arc::new(move |event| {
        let Some(inner) = inner.upgrade() else { return };
        let mut inner = inner.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if inner.child.as_ref().map(|(current, _)| *current) != Some(generation) {
            return;
        }
        match event {
            FollowerEvent::Message(message) => {
                if js_truthy(message.as_object().and_then(|fields| fields.get("ready"))) {
                    inner.status.state = "running";
                }
            }
            // 처리하지 않은 'error'가 백엔드 전체를 끝내지 않게 실패 상태로만 남긴다.
            FollowerEvent::Error(message) => {
                inner.child = None;
                inner.status = FollowStatus { character_id: Some(character_id.clone()), state: "failed", error: Some(message) };
            }
            FollowerEvent::Exit(code) => {
                inner.child = None;
                inner.status =
                    FollowStatus { character_id: Some(character_id.clone()), state: "failed", error: Some(exit_message(code)) };
            }
        }
    })
}

fn exit_message(code: Option<i32>) -> String {
    let home = voice_home_directory();
    let log = join_path(&[&home, "logs", "follow.log"]);
    match code {
        Some(MISSING_PYTHON_EXIT) => format!("음성 파이썬 환경이 없습니다 ({}).", join_path(&[&home, ".venv"])),
        Some(FOLLOW_BUSY_EXIT) => {
            "음성 따라 읽기가 이미 다른 프로세스에서 실행 중입니다. 기존 실행을 종료한 뒤 다시 켜 주세요.".into()
        }
        Some(MODEL_START_FAILURE_EXIT) => format!("음성 모델 초기화 또는 예열에 실패했습니다. 기록은 {log}에 있습니다."),
        _ => {
            let code = code.map_or_else(|| "null".to_string(), |code| code.to_string());
            format!("음성 따라 읽기가 종료되었습니다 (코드 {code}). 기록은 {log}에 있습니다.")
        }
    }
}

/// 터미널 훅처럼 백엔드 실행 파일과 같은 폴더의 따라 읽기 실행 파일.
pub fn default_follower_path() -> String {
    let directory = std::env::current_exe()
        .ok()
        .and_then(|path| path.parent().map(|parent| parent.to_string_lossy().into_owned()))
        .unwrap_or_else(|| ".".into());
    join_path(&[&directory, FOLLOWER_COMMAND])
}

/// Node spawnVoiceFollower의 workingDirectory 옵션. 없으면 띄울 때마다 음성 폴더를 다시 읽는다.
pub fn process_spawner(executable: String, working_directory: Option<String>) -> SpawnFollower {
    Arc::new(move |character_id, events| {
        let working_directory = working_directory.clone().unwrap_or_else(voice_home_directory);
        spawn_process_follower(&executable, &working_directory, character_id, events)
    })
}

fn fs_message(error: &io::Error, syscall: &str, path: &str) -> String {
    match error.raw_os_error() {
        Some(libc::EEXIST) => format!("EEXIST: file already exists, {syscall} '{path}'"),
        _ => node_fs_message(error, syscall, path),
    }
}

struct ProcessFollower {
    /// 거둬들이기 전까지만 번호가 있다. 끝난 뒤 다른 프로세스에 신호를 보내지 않는다.
    pid: Arc<Mutex<Option<i32>>>,
}

impl FollowerHandle for ProcessFollower {
    fn kill(&mut self) {
        if let Some(pid) = *self.pid.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) {
            // SAFETY: 아직 거두지 않은 자식 번호에 종료 신호만 보낸다.
            unsafe {
                libc::kill(pid, libc::SIGTERM);
            }
        }
    }
}

/// spawnVoiceFollower. 작업 폴더 logs/follow.log에 표준출력·오류를 덧붙이고, 표준입력은 닫는다.
/// Node fork의 IPC 대신 유닉스 소켓 한쪽을 자식에게 넘겨 준비 신호를 받고, 백엔드가 끝나면 자식이 닫힘을 본다.
fn spawn_process_follower(
    executable: &str,
    working_directory: &str,
    character_id: &str,
    events: FollowerEvents,
) -> Result<Box<dyn FollowerHandle>, String> {
    let working_directory = resolve_path(working_directory);
    let log_directory = join_path(&[&working_directory, "logs"]);
    std::fs::create_dir_all(&log_directory).map_err(|error| fs_message(&error, "mkdir", &log_directory))?;
    let log_path = join_path(&[&log_directory, "follow.log"]);
    let log = OpenOptions::new()
        .append(true)
        .create(true)
        .open(&log_path)
        .map_err(|error| fs_message(&error, "open", &log_path))?;
    let stdout = log.try_clone().map_err(|error| spawn_error_message(executable, &error))?;
    let (parent_channel, child_channel) =
        std::os::unix::net::UnixStream::pair().map_err(|error| spawn_error_message(executable, &error))?;
    parent_channel.set_nonblocking(true).map_err(|error| spawn_error_message(executable, &error))?;
    let parent_channel =
        tokio::net::UnixStream::from_std(parent_channel).map_err(|error| spawn_error_message(executable, &error))?;
    let channel_fd = child_channel.as_raw_fd();
    let mut command = tokio::process::Command::new(executable);
    command
        .args(["--follow", character_id])
        .current_dir(&working_directory)
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(log))
        .env(CHANNEL_FD_ENV, channel_fd.to_string());
    // SAFETY: fork 뒤 exec 전에 비동기 신호 안전 함수인 fcntl만 부른다. 자식 사본만 exec 뒤에도 열려 있게 한다.
    unsafe {
        command.pre_exec(move || {
            if libc::fcntl(channel_fd, libc::F_SETFD, 0) == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let spawned = command.spawn();
    drop(child_channel);
    let mut child = match spawned {
        Ok(child) => child,
        Err(error) => {
            let message = spawn_error_message(executable, &error);
            // Node는 이 다섯 오류만 'error' 사건으로 늦게 알리고, 나머지는 바로 던진다.
            if matches!(uv_error(&error).0, "EACCES" | "EAGAIN" | "EMFILE" | "ENFILE" | "ENOENT") {
                tokio::spawn(async move { events(FollowerEvent::Error(message)) });
                return Ok(Box::new(ProcessFollower { pid: Arc::default() }));
            }
            return Err(message);
        }
    };
    let pid = Arc::new(Mutex::new(child.id().and_then(|pid| i32::try_from(pid).ok())));
    let message_events = events.clone();
    tokio::spawn(async move {
        let mut lines = BufReader::new(parent_channel).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            if let Ok(message) = serde_json::from_str::<Value>(&line) {
                message_events(FollowerEvent::Message(message));
            }
        }
    });
    let reaped = pid.clone();
    tokio::spawn(async move {
        let status = child.wait().await;
        *reaped.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
        events(FollowerEvent::Exit(status.ok().and_then(|status| status.code())));
    });
    Ok(Box::new(ProcessFollower { pid }))
}
