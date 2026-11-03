// Antigravity 터미널 감시: 대화 SQLite의 steps 표를 증분으로 읽어 질문·답변·사용량으로 터미널 턴을 기록한다.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs;
use std::path::Path;
use std::sync::Arc;

use notify::{RecursiveMode, Watcher as _};
use serde_json::json;
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use tokio::time::{Instant, MissedTickBehavior, interval, sleep_until};

use super::launch::{StepRow, TerminalStep, parse_antigravity_terminal_step};
use super::{Shared, TerminalRuntime, TerminalSession, TerminalTurnCompletion, progress};
use crate::agent_runtime::service::AgentError;
use crate::antigravity_local_state::{StepMetadata, TokenUsage, is_session_id, open_conversation, sqlite_value};
use crate::js::{join_path, now_milliseconds};
use crate::local_artifacts::modified_milliseconds;
use crate::structured_turn_result::read_structured_turn_result;

const STEP_COLUMNS: &str = "SELECT idx, step_type, status, metadata, step_payload FROM steps";

/// 마지막 사용자 경계부터의 질문 하나. 응답 뒤 background task 알림으로 같은 질문의 후속 응답이 올 수 있어
/// running 잠금과 별개로 다음 질문까지 보존한다.
struct CurrentTurn {
    turn_id: String,
    user_index: i64,
    response_index: i64,
    response: Option<(String, Option<StepMetadata>)>,
    /// Map 삽입 순서. 같은 idx를 다시 넣으면 자리는 그대로 두고 값만 바꾼다.
    usage_by_index: Vec<(i64, TokenUsage)>,
    published: bool,
    dirty: bool,
}

impl CurrentTurn {
    fn set_usage(&mut self, index: i64, usage: TokenUsage) {
        match self.usage_by_index.iter_mut().find(|(existing, _)| *existing == index) {
            Some(entry) => entry.1 = usage,
            None => self.usage_by_index.push((index, usage)),
        }
    }
}

/// Node AntigravityTerminalWatcher의 읽기 위치. 세션 turn 잠금 안에서만 바꾼다.
pub(crate) struct AntigravityCursor {
    conversations_root: String,
    path: Option<String>,
    pub(super) last_index: i64,
    pub(super) pending: BTreeSet<i64>,
    current: Option<CurrentTurn>,
    baseline: HashMap<String, f64>,
}

fn step_row(row: &rusqlite::Row) -> rusqlite::Result<StepRow> {
    Ok(StepRow::new(sqlite_value(row, 0)?, sqlite_value(row, 1)?, sqlite_value(row, 2)?, sqlite_value(row, 3)?, sqlite_value(row, 4)?))
}

/// maximumIndex. 어떤 오류든 -1이다.
fn maximum_index(path: &str) -> i64 {
    let read = || -> rusqlite::Result<f64> {
        let connection = open_conversation(path)?;
        connection.query_row("SELECT COALESCE(max(idx), -1) AS idx FROM steps", [], |row| Ok(sqlite_value(row, 0)?.number()))
    };
    read().map_or(-1, |index| index as i64)
}

fn sqlite_failure(error: rusqlite::Error) -> AgentError {
    AgentError::Failed(error.to_string())
}

impl AntigravityCursor {
    pub(crate) fn new(root: &str, external_session_id: Option<&str>) -> Self {
        let conversations_root = join_path(&[root, "conversations"]);
        let path = external_session_id
            .filter(|id| !id.is_empty())
            .map(|id| join_path(&[&conversations_root, &format!("{id}.db")]));
        let last_index = path.as_deref().map_or(-1, maximum_index);
        let mut cursor = Self {
            conversations_root,
            path,
            last_index,
            pending: BTreeSet::new(),
            current: None,
            baseline: HashMap::new(),
        };
        cursor.baseline = cursor.database_files().into_iter().collect();
        cursor
    }

    /// databaseFiles. readdirSync처럼 이름순이고, 폴더를 못 읽거나 stat이 하나라도 실패하면 빈 목록이다.
    fn database_files(&self) -> Vec<(String, f64)> {
        let read = || -> std::io::Result<Vec<(String, f64)>> {
            let mut names = fs::read_dir(&self.conversations_root)?
                .map(|entry| entry.map(|entry| entry.file_name().to_string_lossy().into_owned()))
                .collect::<std::io::Result<Vec<_>>>()?;
            names.sort();
            names
                .into_iter()
                .filter(|name| name.ends_with(".db"))
                .map(|name| {
                    let path = join_path(&[&self.conversations_root, &name]);
                    let modified = modified_milliseconds(&fs::metadata(&path)?);
                    Ok((path, modified))
                })
                .collect()
        };
        read().unwrap_or_default()
    }

    /// forgetCurrentTurn. 앱 중단 뒤 지난 질문의 후속 응답을 다시 붙이지 않는다.
    pub(crate) fn forget_current_turn(&mut self) {
        self.current = None;
        self.pending.clear();
    }

    /// adoptConversationIfNeeded. 새 대화는 터미널을 연 뒤 새로 생기거나 바뀐 DB 중 가장 최근 것이다.
    async fn adopt<R: TerminalRuntime>(&mut self, shared: &Shared<R>, session: &TerminalSession) -> Result<bool, AgentError> {
        if self.path.is_some() {
            return Ok(true);
        }
        let mut candidate: Option<(String, f64)> = None;
        for (path, modified) in self.database_files() {
            let fresh = self.baseline.get(&path).is_none_or(|baseline| modified > baseline.max(session.started_at));
            if fresh && candidate.as_ref().is_none_or(|(_, best)| modified > *best) {
                candidate = Some((path, modified));
            }
        }
        let Some((path, _)) = candidate else {
            return Ok(false);
        };
        let name = Path::new(&path).file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_default();
        let id = name.strip_suffix(".db").filter(|id| !id.is_empty()).unwrap_or(&name).to_string();
        if !is_session_id(&id) {
            return Ok(false);
        }
        shared.runtime.bind_terminal_external_session(&session.character_id, &session.session_id, &id).await?;
        session.lock_state().external_session_id = Some(id);
        self.path = Some(path);
        self.last_index = -1;
        Ok(true)
    }

    /// rows. 커서 뒤 행과, 미완료로 보았던 행을 다시 읽는다. 같은 idx는 뒤에 읽은 행이 이긴다.
    fn rows(&self) -> rusqlite::Result<Vec<StepRow>> {
        let Some(path) = self.path.as_deref() else {
            return Ok(Vec::new());
        };
        let connection = open_conversation(path)?;
        let mut rows = BTreeMap::new();
        let mut statement = connection.prepare(&format!("{STEP_COLUMNS} WHERE idx > ? ORDER BY idx"))?;
        for row in statement.query_map([self.last_index as f64], step_row)? {
            let row = row?;
            rows.insert(row.index, row);
        }
        if !self.pending.is_empty() {
            let placeholders = vec!["?"; self.pending.len()].join(", ");
            let mut statement = connection.prepare(&format!("{STEP_COLUMNS} WHERE idx IN ({placeholders})"))?;
            let values = self.pending.iter().map(|index| *index as f64);
            for row in statement.query_map(rusqlite::params_from_iter(values), step_row)? {
                let row = row?;
                rows.insert(row.index, row);
            }
        }
        Ok(rows.into_values().collect())
    }

    /// sweep. 세션 turn 잠금을 쥔 채 부른다.
    async fn sweep<R: TerminalRuntime>(&mut self, shared: &Shared<R>, session: &TerminalSession) -> Result<(), AgentError> {
        let closed = session.lock_state().closed;
        if closed || !self.adopt(shared, session).await? {
            return Ok(());
        }
        for row in self.rows().map_err(sqlite_failure)? {
            let index = row.index;
            self.last_index = self.last_index.max(index);
            if self.current.as_ref().is_some_and(|current| index < current.user_index) {
                self.pending.remove(&index);
                continue;
            }
            // Antigravity는 한 idx의 행을 먼저 미완료로 INSERT한 뒤 UPDATE한다. 커서만 넘기면 완료된 같은 행을 다시 볼 수 없어 따로 다시 읽는다.
            if row.status != 3.0 {
                // 실패·중단된 도구 행은 완료로 UPDATE되지 않는다.
                if row.status == 6.0 || row.status == 7.0 {
                    self.pending.remove(&index);
                } else {
                    self.pending.insert(index);
                }
                // 미완료 사용자 행 뒤 응답을 이전 질문에 붙이지 않는다.
                if row.step_type == 14.0 {
                    break;
                }
                continue;
            }
            let Some(event) = parse_antigravity_terminal_step(&row) else {
                self.pending.remove(&index);
                continue;
            };
            let (activities, metadata, assistant) = match event {
                TerminalStep::User(text) => {
                    self.pending.insert(index);
                    self.publish_current_turn(shared, session).await?;
                    let running = session.lock_state().running_turn_id.clone();
                    if let Some(running) = running {
                        shared.runtime.interrupt_terminal_turn(&session.character_id, &running).await?;
                        session.lock_state().running_turn_id = None;
                    }
                    if self.current.is_some() {
                        shared.reset(session)?;
                    }
                    let turn_id = shared.begin_dispatched_turn(session, text, None).await?;
                    self.current = Some(CurrentTurn {
                        turn_id,
                        user_index: index,
                        response_index: -1,
                        response: None,
                        usage_by_index: Vec::new(),
                        published: false,
                        dirty: false,
                    });
                    self.pending.remove(&index);
                    continue;
                }
                TerminalStep::Assistant { text, metadata, activities } => (activities, metadata, Some(text)),
                TerminalStep::Activity { activities, metadata } => (activities, metadata, None),
            };
            // 모드 진입 전의 고아 단계는 미래의 새 질문에 이월하지 않는다.
            let Some(current) = self.current.as_mut() else {
                self.pending.remove(&index);
                continue;
            };
            {
                let mut state = session.lock_state();
                for activity in &activities {
                    state.activities.add(activity);
                }
                if let Some(text) = &assistant {
                    state.activities.add(&json!({ "kind": "message", "text": text, "eventKey": format!("step:{index}:message") }));
                }
            }
            if let Some(metadata) = &metadata {
                current.set_usage(index, metadata.usage);
            }
            current.dirty = true;
            if let Some(text) = assistant
                && index > current.response_index
            {
                current.response_index = index;
                current.response = Some((text, metadata));
            }
            self.pending.remove(&index);
        }
        progress::publish(shared, session);
        self.publish_current_turn(shared, session).await
    }

    /// publishCurrentTurn. 같은 질문의 전체 스냅샷으로 덮어쓴다. 재시도·후속 응답의 토큰을 더하기만 하거나
    /// 마지막 응답 한 단계의 사용량만 남기지 않는다.
    async fn publish_current_turn<R: TerminalRuntime>(&mut self, shared: &Shared<R>, session: &TerminalSession) -> Result<(), AgentError> {
        let Some(turn) = self.current.as_mut() else {
            return Ok(());
        };
        let Some((text, metadata)) = turn.response.as_ref().filter(|_| turn.dirty) else {
            return Ok(());
        };
        let text = text.clone();
        let at = metadata.as_ref().map(|metadata| metadata.at).filter(|at| *at != 0.0 && !at.is_nan());
        let mut totals = TokenUsage::default();
        for (_, usage) in &turn.usage_by_index {
            totals.input_tokens += usage.input_tokens;
            totals.output_tokens += usage.output_tokens;
            totals.cached_input_tokens += usage.cached_input_tokens;
            totals.reasoning_output_tokens += usage.reasoning_output_tokens;
        }
        let usage = (!turn.usage_by_index.is_empty()).then(|| {
            json!({
                "inputTokens": totals.input_tokens,
                "outputTokens": totals.output_tokens,
                "cachedInputTokens": totals.cached_input_tokens,
                "reasoningOutputTokens": totals.reasoning_output_tokens,
                "cacheWriteInputTokens": null,
                "cacheWrite5mInputTokens": null,
                "cacheWrite1hInputTokens": null,
                "requestUsages": turn.usage_by_index.iter().map(|(_, usage)| json!({ "usage": usage })).collect::<Vec<_>>(),
            })
        });
        let (initial_generated_images, structured_path, activities) = {
            let state = session.lock_state();
            (state.initial_generated_images.clone(), state.structured_result_path.clone(), state.activities.finish(&text))
        };
        let completion = TerminalTurnCompletion {
            character_id: session.character_id.clone(),
            turn_id: turn.turn_id.clone(),
            response: text,
            ended_at: Some(at.unwrap_or_else(now_milliseconds)),
            usage,
            initial_generated_images,
            structured: Some(read_structured_turn_result(structured_path.as_deref())),
            activities,
            refresh_completed: turn.published,
            ..TerminalTurnCompletion::default()
        };
        shared.complete_turn(session, completion).await?;
        turn.published = true;
        turn.dirty = false;
        session.lock_state().running_turn_id = None;
        Ok(())
    }
}

pub(super) async fn sweep_locked<R: TerminalRuntime>(shared: &Shared<R>, session: &TerminalSession) -> Result<(), AgentError> {
    let mut context = session.turn.lock().await;
    match context.antigravity.as_mut() {
        Some(cursor) => cursor.sweep(shared, session).await,
        None => Ok(()),
    }
}

/// 폴더·요약 DB 감시와 주기 확인을 도는 작업.
pub(crate) struct AntigravityWatcher {
    stop: watch::Sender<bool>,
    handle: JoinHandle<()>,
}

/// start. SQLite가 WAL에만 기록하면 macOS 파일 감시가 마지막 변경을 놓칠 수 있다.
/// 주기 확인은 debounce를 거치지 않아 끊이지 않는 WAL 알림에도 굶지 않는다.
pub(crate) fn start<R: TerminalRuntime>(shared: &Arc<Shared<R>>, session: &Arc<TerminalSession>) -> AntigravityWatcher {
    let root = shared.options.antigravity_root();
    let (events, receiver) = mpsc::unbounded_channel();
    let watchers: Vec<notify::RecommendedWatcher> = [join_path(&[&root, "conversations"]), join_path(&[&root, "conversation_summaries.db"])]
        .iter()
        .filter_map(|path| {
            let events = events.clone();
            let mut watcher = notify::recommended_watcher(move |result: notify::Result<notify::Event>| {
                if result.is_ok() {
                    let _ = events.send(());
                }
            })
            .ok()?;
            watcher.watch(Path::new(path), RecursiveMode::NonRecursive).ok()?;
            Some(watcher)
        })
        .collect();
    let (stop, stopped) = watch::channel(false);
    let handle = tokio::spawn(run(Arc::clone(shared), Arc::clone(session), watchers, receiver, stopped));
    AntigravityWatcher { stop, handle }
}

async fn run<R: TerminalRuntime>(
    shared: Arc<Shared<R>>,
    session: Arc<TerminalSession>,
    watchers: Vec<notify::RecommendedWatcher>,
    mut events: mpsc::UnboundedReceiver<()>,
    mut stopped: watch::Receiver<bool>,
) {
    let _watchers = watchers;
    let debounce = shared.options.antigravity_debounce;
    let mut poll = interval(shared.options.antigravity_poll_interval);
    poll.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut deadline: Option<Instant> = None;
    loop {
        let sweep = tokio::select! {
            biased;
            _ = stopped.changed() => break,
            _ = poll.tick() => true,
            _ = sleep_until(deadline.unwrap_or_else(Instant::now)), if deadline.is_some() => {
                deadline = None;
                true
            }
            Some(()) = events.recv() => {
                deadline = Some(Instant::now() + debounce);
                false
            }
        };
        if sweep && let Err(error) = sweep_locked(&shared, &session).await {
            eprintln!("Antigravity 터미널 기록 해독 실패: {error}");
        }
    }
}

impl AntigravityWatcher {
    /// stop. 감시를 끝내고 진행 중인 확인을 기다린다. 닫을 때는 마지막으로 한 번 더 읽고 그 오류를 올린다.
    pub(crate) async fn stop<R: TerminalRuntime>(
        self,
        shared: &Arc<Shared<R>>,
        session: &Arc<TerminalSession>,
        final_sweep: bool,
    ) -> Result<(), AgentError> {
        let _ = self.stop.send(true);
        let _ = self.handle.await;
        if final_sweep {
            sweep_locked(shared, session).await?;
        }
        Ok(())
    }
}
