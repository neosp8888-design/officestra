// Codex 터미널 감시: notify는 완료 때만 오므로 rollout을 파일 끝부터 증분으로 읽어 running 턴을 먼저 만든다.
// 완료와 답변 추출은 notify 경로가 맡는다. 터미널이 열린 동안 그 직원의 GUI 턴은 막혀 여기서 붙는 턴은 모두 터미널 턴이다.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::sync::Arc;

use serde_json::{Value, json};
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio::time::{Instant, MissedTickBehavior, interval_at};

use super::launch::date_from_epoch;
use super::progress::read_at;
use super::{Shared, TerminalRuntime, TerminalSession, progress};
use crate::codex_rollout_turns::{codex_rollout_user_prompt, find_codex_rollout_path, rollout_path_cache};
use crate::js::{js_string_or_empty, js_to_number, js_trim, js_truthy, node_fs_message, now_milliseconds, parse_js_json};

/// Node CodexTerminalWatcher의 읽기 위치. 세션 turn 잠금 안에서만 바꾼다.
pub(crate) struct CodexCursor {
    sessions_root: String,
    path: Option<String>,
    offset: u64,
    remainder: String,
    /// task_started로 본 턴 ID와 그 started_at(없으면 None).
    pending_starts: HashMap<String, Option<Value>>,
    seen: HashSet<String>,
    /// observedTurnID. None은 null이고, turn_context가 준 값은 문자열이 아닐 수도 있다.
    observed: Option<Value>,
}

fn turn_id_text(payload: &Value) -> String {
    js_trim(&js_string_or_empty(payload.get("turn_id"))).to_string()
}

impl CodexCursor {
    pub(crate) fn new(sessions_root: String) -> Self {
        Self {
            sessions_root,
            path: None,
            offset: 0,
            remainder: String::new(),
            pending_starts: HashMap::new(),
            seen: HashSet::new(),
            observed: None,
        }
    }

    /// markCompleted. notify 경로가 기록한 턴은 늦게 읽혀도 다시 만들지 않는다.
    pub(crate) fn mark_completed(&mut self, turn_id: &str) {
        let id = js_trim(turn_id);
        if id.is_empty() {
            return;
        }
        self.pending_starts.remove(id);
        self.seen.insert(id.to_string());
    }

    /// adoptRolloutIfNeeded. 세션이 묶이는 시점의 파일 끝에서 시작해 지난 기록을 되풀이하지 않는다.
    fn adopt(&mut self, session: &TerminalSession) -> Result<bool, String> {
        if self.path.is_some() {
            return Ok(true);
        }
        let Some(external) = session.lock_state().external_session_id.clone().filter(|id| !id.is_empty()) else {
            return Ok(false);
        };
        let found = find_codex_rollout_path(Some(&external), &self.sessions_root, now_milliseconds(), rollout_path_cache())
            .map_err(|error| error.to_string())?;
        let Some(path) = found else {
            return Ok(false);
        };
        self.offset = fs::metadata(&path).map_err(|error| node_fs_message(&error, "stat", &path))?.len();
        self.path = Some(path);
        self.remainder.clear();
        Ok(true)
    }

    /// readNewLines. 파일이 줄면 처음부터 다시 읽는다. 조각마다 따로 UTF-8로 푼다(Buffer.toString).
    fn read_new_lines(&mut self) -> Result<Vec<String>, String> {
        let Some(path) = self.path.clone() else {
            return Ok(Vec::new());
        };
        let size = fs::metadata(&path).map_err(|error| node_fs_message(&error, "stat", &path))?.len();
        if size < self.offset {
            self.offset = 0;
            self.remainder.clear();
        }
        if size == self.offset {
            return Ok(Vec::new());
        }
        let bytes = read_at(&path, self.offset, size - self.offset).map_err(|error| node_fs_message(&error, "open", &path))?;
        self.offset = size;
        let text = format!("{}{}", self.remainder, String::from_utf8_lossy(&bytes));
        let mut lines: Vec<String> = text.split('\n').map(str::to_string).collect();
        self.remainder = lines.pop().unwrap_or_default();
        Ok(lines)
    }

    /// sweep. 세션 turn 잠금을 쥔 채 부른다.
    async fn sweep<R: TerminalRuntime>(&mut self, shared: &Shared<R>, session: &TerminalSession) -> Result<(), String> {
        let closed = session.lock_state().closed;
        if closed || !self.adopt(session)? {
            return Ok(());
        }
        let empty = json!({});
        for line in self.read_new_lines()? {
            if line.is_empty() {
                continue;
            }
            let Ok(record) = parse_js_json(&line) else {
                continue;
            };
            let payload = record.get("payload").filter(|payload| !payload.is_null()).unwrap_or(&empty);
            let event_type = (record.get("type").and_then(Value::as_str) == Some("event_msg"))
                .then(|| payload.get("type").and_then(Value::as_str))
                .flatten();
            match event_type {
                Some("task_started") => {
                    let id = turn_id_text(payload);
                    self.observed = Some(Value::String(id.clone()));
                    if !id.is_empty() && !self.seen.contains(&id) {
                        self.pending_starts.insert(id, payload.get("started_at").cloned());
                    }
                    continue;
                }
                Some("task_complete") => {
                    // 질문을 보기 전에 끝난 턴은 notify 경로가 처음부터 기록한다.
                    self.pending_starts.remove(&turn_id_text(payload));
                    self.observed = None;
                    continue;
                }
                Some("turn_aborted") => {
                    let id = turn_id_text(payload);
                    self.pending_starts.remove(&id);
                    self.seen.insert(id.clone());
                    self.observed = None;
                    let running = {
                        let state = session.lock_state();
                        state
                            .running_turn_id
                            .clone()
                            .filter(|_| !id.is_empty() && state.codex_turn_id.as_deref() == Some(id.as_str()))
                    };
                    if let Some(running) = running {
                        shared
                            .runtime
                            .interrupt_terminal_turn(&session.character_id, &running)
                            .await
                            .map_err(|error| error.to_string())?;
                        {
                            let mut state = session.lock_state();
                            state.running_turn_id = None;
                            state.codex_turn_id = None;
                        }
                        shared.reset(session).map_err(|error| error.to_string())?;
                        shared.changed(session);
                    }
                    continue;
                }
                _ => {}
            }
            if record.get("type").and_then(Value::as_str) == Some("turn_context") && js_truthy(payload.get("turn_id")) {
                self.observed = payload.get("turn_id").cloned();
            }
            {
                let mut state = session.lock_state();
                let codex = state.codex_turn_id.clone().map_or(Value::Null, Value::String);
                let observed = self.observed.clone().unwrap_or(Value::Null);
                let turn = payload.get("turn_id");
                if state.running_turn_id.is_some() && observed == codex && (!js_truthy(turn) || turn == Some(&codex)) {
                    state.activities.codex(&record)?;
                }
            }
            let Some(prompt) = codex_rollout_user_prompt(&record) else {
                continue;
            };
            let Some(started) = self.pending_starts.remove(&prompt.turn_id) else {
                continue;
            };
            self.seen.insert(prompt.turn_id.clone());
            let running = session.lock_state().running_turn_id.is_some();
            if running {
                continue;
            }
            // 한 턴을 못 만들어도 같은 묶음의 뒷줄은 계속 읽는다. 이미 읽은 바이트는 다시 오지 않는다.
            let started_at = date_from_epoch(started.as_ref().map_or(f64::NAN, js_to_number));
            match shared.begin_dispatched_turn(session, prompt.text, Some(started_at)).await {
                Ok(_) => {
                    session.lock_state().codex_turn_id = Some(prompt.turn_id);
                    shared.changed(session);
                }
                Err(error) => eprintln!("Codex 터미널 running 턴을 만들지 못했습니다({}): {error}", session.character_id),
            }
        }
        progress::publish(shared, session);
        Ok(())
    }
}

/// 세션 turn 잠금을 잡고 한 번 읽는다.
pub(super) async fn sweep_locked<R: TerminalRuntime>(shared: &Shared<R>, session: &TerminalSession) -> Result<(), String> {
    let mut context = session.turn.lock().await;
    match context.codex.as_mut() {
        Some(cursor) => cursor.sweep(shared, session).await,
        None => Ok(()),
    }
}

/// 400ms마다 rollout을 읽는 작업. 첫 확인도 한 주기 뒤다(setInterval).
pub(crate) struct CodexWatcher {
    stop: watch::Sender<bool>,
    handle: JoinHandle<()>,
}

pub(crate) fn start<R: TerminalRuntime>(shared: &Arc<Shared<R>>, session: &Arc<TerminalSession>) -> CodexWatcher {
    let (stop, stopped) = watch::channel(false);
    let handle = tokio::spawn(run(Arc::clone(shared), Arc::clone(session), stopped));
    CodexWatcher { stop, handle }
}

async fn run<R: TerminalRuntime>(shared: Arc<Shared<R>>, session: Arc<TerminalSession>, mut stopped: watch::Receiver<bool>) {
    let period = shared.options.codex_poll_interval;
    let mut timer = interval_at(Instant::now() + period, period);
    timer.set_missed_tick_behavior(MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            biased;
            _ = stopped.changed() => break,
            _ = timer.tick() => {
                if let Err(error) = sweep_locked(&shared, &session).await {
                    eprintln!("Codex 터미널 기록을 읽지 못했습니다. {error}");
                }
            }
        }
    }
}

impl CodexWatcher {
    /// stop. 완료는 notify가 맡으므로 마지막 확인은 하지 않는다. 닫히는 중에 턴을 만들었다가 곧바로 중단하는 낭비를 피한다.
    pub(crate) async fn stop(self) {
        let _ = self.stop.send(true);
        let _ = self.handle.await;
    }
}
