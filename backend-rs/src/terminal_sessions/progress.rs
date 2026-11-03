// 터미널 말풍선의 현재 작업 표시: Claude 원본 증분을 읽어 상태만 방송한다. 활동·요약을 DB에 쓰지 않는다.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use notify::{RecursiveMode, Watcher as _};
use serde_json::{Value, json};
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use tokio::time::{Instant, sleep_until};

use super::{ClaudeTranscript, PublishedProgress, Shared, TerminalRuntime, TerminalSession};
use crate::agent_event_parser::parse_agent_event;
use crate::js::{date_parse, js_string, js_truthy, parse_js_json, utf16_prefix};

const MAX_READ: u64 = 1024 * 1024;
const DEBOUNCE: Duration = Duration::from_millis(150);

/// publishTerminalProgress. 수집기의 가장 최근 활동이 바뀌었을 때만 짧은 상태를 내보낸다.
pub(crate) fn publish<R: TerminalRuntime>(shared: &Shared<R>, session: &TerminalSession) {
    let event = {
        let mut state = session.lock_state();
        let Some(turn_id) = state.running_turn_id.clone() else {
            return;
        };
        if state.closed || state.tool_events_closing {
            return;
        }
        let (generation, revision) = (state.collector_generation, state.activities.revision());
        if state
            .published
            .as_ref()
            .is_some_and(|published| published.turn_id == turn_id && published.generation == generation && published.revision == revision)
        {
            return;
        }
        let Some(latest) = state.activities.latest() else {
            return;
        };
        // 글·추론 본문은 짧은 상태 표시에 필요 없다.
        let text = if matches!(latest.kind, "message" | "thinking") { String::new() } else { utf16_prefix(&latest.text, 600) };
        let work = json!({ "kind": latest.kind, "text": text, "status": latest.status });
        let signature = serde_json::to_string(&work).unwrap_or_default();
        let changed = state
            .published
            .as_ref()
            .is_none_or(|published| published.turn_id != turn_id || published.signature != signature);
        state.published = Some(PublishedProgress { turn_id: turn_id.clone(), generation, revision, signature });
        changed.then(|| {
            json!({
                "type": "terminal.work-status",
                "characterId": session.character_id,
                "turnId": turn_id,
                "workActivity": work,
            })
        })
    };
    if let Some(event) = event {
        shared.runtime.broadcast(event);
    }
}

/// StringDecoder("utf8"). 글자 경계에 걸친 바이트는 다음 조각까지 미루고 잘못된 바이트는 U+FFFD다.
#[derive(Default)]
pub(crate) struct Utf8Stream {
    pending: Vec<u8>,
}

impl Utf8Stream {
    pub(crate) fn push(&mut self, bytes: &[u8]) -> String {
        self.pending.extend_from_slice(bytes);
        let mut text = String::new();
        let mut rest = self.pending.as_slice();
        loop {
            match std::str::from_utf8(rest) {
                Ok(valid) => {
                    text.push_str(valid);
                    rest = &[];
                    break;
                }
                Err(error) => {
                    let (valid, invalid) = rest.split_at(error.valid_up_to());
                    text.push_str(std::str::from_utf8(valid).unwrap_or_default());
                    match error.error_len() {
                        Some(length) => {
                            text.push('\u{FFFD}');
                            rest = &invalid[length..];
                        }
                        None => {
                            rest = invalid;
                            break;
                        }
                    }
                }
            }
        }
        self.pending = rest.to_vec();
        text
    }
}

pub(crate) fn read_at(path: &str, offset: u64, length: u64) -> std::io::Result<Vec<u8>> {
    let mut file = File::open(path)?;
    file.seek(SeekFrom::Start(offset))?;
    let mut bytes = Vec::with_capacity(length as usize);
    file.take(length).read_to_end(&mut bytes)?;
    Ok(bytes)
}

pub(super) struct Cursor {
    turn_id: String,
    path: String,
    offset: u64,
    inode: Option<u64>,
    started_at: String,
    remainder: String,
    decoder: Utf8Stream,
}

impl Cursor {
    /// 원본 경로나 시작 위치를 모르면 None이다.
    pub(super) fn new(turn_id: String, source: ClaudeTranscript) -> Option<Self> {
        Some(Self {
            turn_id,
            path: source.path?,
            offset: source.offset?,
            inode: source.inode,
            started_at: source.started_at,
            remainder: String::new(),
            decoder: Utf8Stream::default(),
        })
    }

    /// 한 번 읽는다. 아직 읽을 바이트가 남았으면 true다.
    pub(super) fn sweep<R: TerminalRuntime>(&mut self, shared: &Shared<R>, session: &TerminalSession) -> Result<bool, String> {
        {
            let state = session.lock_state();
            if state.closed || state.running_turn_id.as_deref() != Some(self.turn_id.as_str()) || state.tool_events_closing {
                return Ok(false);
            }
        }
        let Ok(metadata) = std::fs::metadata(&self.path) else {
            return Ok(false);
        };
        // 압축·교체로 앞쪽에 지난 턴이 붙을 수 있다. 지난 기록을 진행 중 작업으로 다시 보이지 않고 최종 가져오기에 맡긴다.
        if self.inode.is_some_and(|inode| metadata.ino() != inode) || metadata.len() <= self.offset {
            return Ok(false);
        }
        let length = (metadata.len() - self.offset).min(MAX_READ);
        let bytes = read_at(&self.path, self.offset, length).map_err(|error| error.to_string())?;
        self.offset += bytes.len() as u64;
        let text = format!("{}{}", self.remainder, self.decoder.push(&bytes));
        let mut lines: Vec<&str> = text.split('\n').collect();
        self.remainder = lines.pop().unwrap_or_default().to_string();
        let started = date_parse(&self.started_at);
        {
            let mut state = session.lock_state();
            let external = state.external_session_id.clone();
            for line in lines {
                let Ok(record) = parse_js_json(line) else {
                    continue;
                };
                if record.is_null() {
                    return Err("Cannot read properties of null (reading 'isSidechain')".into());
                }
                if js_truthy(record.get("isSidechain")) {
                    continue;
                }
                if let Some(session_id) = record.get("sessionId").filter(|value| js_truthy(Some(value)))
                    && (session_id.as_str().is_none() || session_id.as_str() != external.as_deref())
                {
                    continue;
                }
                let at = record.get("timestamp").and_then(|value| date_parse(&js_string(value)));
                if let (Some(at), Some(started)) = (at, started)
                    && at < started
                {
                    continue;
                }
                if !matches!(record.get("type").and_then(Value::as_str), Some("assistant" | "user")) {
                    continue;
                }
                let parsed = parse_agent_event(line, "claude", Some(&session.workdir))?;
                state.activities.parsed(parsed.as_ref());
            }
        }
        publish(shared, session);
        Ok(self.offset < metadata.len())
    }
}

/// ClaudeTerminalProgressWatcher. 진행 중인 턴 동안 그 원본이 있는 폴더만 감시한다. 유휴 주기 확인은 없다.
pub(crate) struct ProgressWatcher {
    stop: watch::Sender<bool>,
    handle: JoinHandle<()>,
}

impl ProgressWatcher {
    /// 원본 경로나 시작 위치를 모르거나 폴더 감시를 못 걸면 None이다. 최종 원본 가져오기가 대신한다.
    pub(crate) fn start<R: TerminalRuntime>(shared: &Arc<Shared<R>>, session: &Arc<TerminalSession>) -> Option<Self> {
        let (turn_id, source) = {
            let state = session.lock_state();
            (state.running_turn_id.clone()?, state.claude_transcript.clone()?)
        };
        let cursor = Cursor::new(turn_id, source)?;
        let path = Path::new(&cursor.path);
        let name = path.file_name().map(ToOwned::to_owned);
        let directory = path.parent().filter(|parent| !parent.as_os_str().is_empty()).unwrap_or(Path::new("."));
        let (events, receiver) = mpsc::unbounded_channel();
        let failed = Arc::new(AtomicBool::new(false));
        let mut watcher = notify::recommended_watcher(move |result: notify::Result<notify::Event>| {
            // Node watcher.on("error")는 감시를 닫는다.
            let Ok(event) = result else {
                failed.store(true, Ordering::SeqCst);
                return;
            };
            if failed.load(Ordering::SeqCst) {
                return;
            }
            if !event.paths.is_empty() && !event.paths.iter().any(|changed| changed.file_name() == name.as_deref()) {
                return;
            }
            let _ = events.send(());
        })
        .ok()?;
        watcher.watch(directory, RecursiveMode::NonRecursive).ok()?;
        let (stop, stopped) = watch::channel(false);
        let handle = tokio::spawn(run(Arc::clone(shared), Arc::clone(session), watcher, receiver, stopped, cursor));
        Some(Self { stop, handle })
    }

    /// stop. 진행 중인 읽기가 끝날 때까지 기다린다.
    pub(crate) async fn stop(self) {
        let _ = self.stop.send(true);
        let _ = self.handle.await;
    }
}

async fn run<R: TerminalRuntime>(
    shared: Arc<Shared<R>>,
    session: Arc<TerminalSession>,
    watcher: notify::RecommendedWatcher,
    mut events: mpsc::UnboundedReceiver<()>,
    mut stopped: watch::Receiver<bool>,
    mut cursor: Cursor,
) {
    let _watcher = watcher;
    let mut deadline = Some(Instant::now());
    loop {
        tokio::select! {
            biased;
            _ = stopped.changed() => break,
            Some(()) = events.recv() => deadline = Some(Instant::now() + DEBOUNCE),
            _ = sleep_until(deadline.unwrap_or_else(Instant::now)), if deadline.is_some() => {
                deadline = None;
                match cursor.sweep(&shared, &session) {
                    Ok(true) if !*stopped.borrow() => deadline = Some(Instant::now()),
                    Ok(_) => {}
                    Err(message) => eprintln!("Claude 터미널 작업 상태 읽기 실패: {message}"),
                }
            }
        }
    }
}
