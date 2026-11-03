// 앱이 소유한 PTY 터미널 세션의 실행 명세, 직원 잠금, 외부 CLI 턴 기록을 관리한다(Node terminal-sessions.mjs).
//
// 잠금 순서: 세션 turn(비동기) → 감시 커서(turn 안에 있음), 실행기 잡 표 → 등록부 → 세션 state.
// 세션 state·등록부는 std Mutex라 await를 건너 쥐지 않는다. 실행기 잡 표는 세션 state를 쥔 채 읽지 않는다.

mod antigravity;
mod codex;
mod dispatch;
mod events;
pub(crate) mod launch;
mod progress;
#[cfg(test)]
mod tests;

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::sync::Mutex as AsyncMutex;

use crate::agent_runtime::launch::{LaunchPaths, RuntimeCharacter};
use crate::agent_runtime::lifecycle::StartRequest;
use crate::agent_runtime::service::{AgentError, RuntimeServices, SessionRoots};
use crate::agent_runtime::turn::{TurnSeed, TurnState};
use crate::agent_runtime::locate_executable;
use crate::codex_rollout_turns::{CodexTerminalTurnGate, GateOptions};
use crate::developer_tool_events::DeveloperToolEvent;
use crate::js::{iso_string, join_path, js_trim, now_milliseconds, random_uuid};
use crate::local_artifacts::list_generated_images;
use crate::local_embedding::Environment;
use crate::local_provider::normalize_local_definition;
use crate::local_provider::service::{EntryRelease, LaunchRequest, LocalProviderService};
use crate::structured_turn_result::{
    StructuredTurnResult, discard_structured_turn_result, prepare_structured_turn_result,
};
use crate::terminal_turn_activities::TerminalActivityCollector;

use antigravity::{AntigravityCursor, AntigravityWatcher};
use codex::{CodexCursor, CodexWatcher};
use dispatch::PendingDispatch;
use launch::{terminal_arguments, terminal_environment};
use progress::ProgressWatcher;

const SESSION_NOT_OPEN: &str = "열린 터미널 세션을 찾을 수 없습니다.";

/// 관리자가 실행기(Node AgentRuntime)에 맡기는 일. 시험은 DB 없는 가짜 실행기를 넣는다.
pub trait TerminalRuntime: Send + Sync + 'static {
    fn is_draining(&self) -> bool;
    /// running·preparing·compacting 중 하나에 이 직원이 있는가.
    fn character_busy(&self, character_id: &str) -> bool;
    fn is_compacting(&self, character_id: &str) -> bool;
    fn broadcast(&self, event: Value);
    /// prepareTerminalLaunch.
    fn prepare_terminal_launch(&self, character_id: &str) -> impl Future<Output = Result<TerminalLaunch, AgentError>> + Send;
    /// bindTerminalExternalSession.
    fn bind_terminal_external_session(
        &self,
        character_id: &str,
        session_id: &str,
        external_session_id: &str,
    ) -> impl Future<Output = Result<(), AgentError>> + Send;
    /// beginTerminalTurn. 만든 턴 ID를 돌려준다.
    fn begin_terminal_turn(&self, start: TerminalTurnStart) -> impl Future<Output = Result<String, AgentError>> + Send;
    /// completeTerminalTurn.
    fn complete_terminal_turn(&self, completion: TerminalTurnCompletion) -> impl Future<Output = Result<(), AgentError>> + Send;
    /// addTerminalTurnReportedCost.
    fn add_terminal_turn_reported_cost(
        &self,
        character_id: &str,
        turn_id: &str,
        cost_usd: f64,
    ) -> impl Future<Output = Result<bool, AgentError>> + Send;
    /// interruptTerminalTurn.
    fn interrupt_terminal_turn(&self, character_id: &str, turn_id: &str) -> impl Future<Output = Result<bool, AgentError>> + Send;
    /// 전달 사전 확인의 pending·running 턴 조회.
    fn terminal_dispatch_busy(&self, character_id: &str) -> impl Future<Output = Result<bool, AgentError>> + Send;
    /// 이 직원의 자동 전달 태그(reply_route_recipients).
    fn reply_route_tags(&self, character_id: &str) -> impl Future<Output = Result<Vec<String>, AgentError>> + Send;
    /// runtime.localProviders. 없으면 로컬 프로필 직원의 터미널이 열리지 않는다.
    fn local_providers(&self) -> Option<LocalProviderService>;
}

/// prepareTerminalLaunch 결과.
#[derive(Debug, Clone)]
pub struct TerminalLaunch {
    pub character: RuntimeCharacter,
    pub session_id: String,
    pub conversation_id: String,
    pub external_session_id: Option<String>,
    pub workdir: String,
}

/// beginTerminalTurn 인자. 답변 전달 수신자(replyRecipientID)는 Node recordReplyDelivery도 쓰지 않아 뺐다.
#[derive(Debug, Clone)]
pub struct TerminalTurnStart {
    pub character_id: String,
    pub session_id: String,
    /// `String(prompt ?? "")`. 실행기가 다듬는다.
    pub prompt: String,
    /// 밀리초. None은 지금이다.
    pub started_at: Option<f64>,
    pub execution: RuntimeCharacter,
    pub sender_character_id: Option<Value>,
    pub request_source: Option<String>,
    pub delivery_id: Option<String>,
}

/// completeTerminalTurn 인자.
#[derive(Debug, Clone, Default)]
pub struct TerminalTurnCompletion {
    pub character_id: String,
    pub turn_id: String,
    pub response: String,
    /// 밀리초. None은 지금이다.
    pub ended_at: Option<f64>,
    pub usage: Option<Value>,
    pub resolved_model: Option<String>,
    pub initial_generated_images: HashSet<String>,
    pub structured: Option<StructuredTurnResult>,
    pub reported_cost_usd: Option<f64>,
    pub activities: Vec<Value>,
    pub refresh_completed: bool,
    /// 개발 도구 이벤트가 이 턴에 활동을 썼다(Node toolActivityState.turnID === turnID). 앞 활동을 되읽는다.
    pub restore_tool_activities: bool,
}

/// messageAvailability가 보는 터미널 상태.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TerminalAvailability {
    pub running: bool,
    pub opening: bool,
    pub closing: bool,
    pub dispatch: bool,
}

/// Node TerminalSessionManager 생성 인자.
#[derive(Debug, Clone)]
pub struct TerminalManagerOptions {
    pub port: u16,
    /// CLI 기록 폴더(~/.codex/sessions, ~/.claude/projects, ~/.gemini/antigravity-cli)와 생성 이미지 폴더.
    pub roots: SessionRoots,
    pub base_environment: Environment,
    pub paths: LaunchPaths,
    /// officestra-terminal-hook 실행 파일.
    pub hook_path: String,
    pub antigravity_poll_interval: Duration,
    pub antigravity_debounce: Duration,
    pub codex_poll_interval: Duration,
    pub dispatch_timeout: Duration,
}

impl TerminalManagerOptions {
    pub fn new(roots: SessionRoots, paths: LaunchPaths, base_environment: Environment, hook_path: String) -> Self {
        Self {
            port: 4317,
            roots,
            base_environment,
            paths,
            hook_path,
            antigravity_poll_interval: Duration::from_millis(1_000),
            antigravity_debounce: Duration::from_millis(500),
            codex_poll_interval: Duration::from_millis(400),
            dispatch_timeout: Duration::from_millis(20_000),
        }
    }

    fn antigravity_root(&self) -> String {
        join_path(&[&self.roots.home, ".gemini", "antigravity-cli"])
    }
}

/// 백엔드 실행 파일과 같은 폴더의 officestra-terminal-hook.
pub fn default_hook_path() -> String {
    let directory = std::env::current_exe()
        .ok()
        .and_then(|path| path.parent().map(|parent| parent.to_string_lossy().into_owned()))
        .unwrap_or_else(|| ".".into());
    join_path(&[&directory, launch::TERMINAL_HOOK_COMMAND])
}

/// Claude 상태줄이 알려 준 세션 누적 비용과 이미 턴에 반영한 몫.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct ClaudeCost {
    pub(crate) reported: Option<f64>,
    pub(crate) settled: f64,
}

/// UserPromptSubmit 때 기억한 Claude 원본 위치.
#[derive(Debug, Clone)]
pub(crate) struct ClaudeTranscript {
    pub(crate) path: Option<String>,
    /// None은 Node의 null(시작 위치 모름)이다.
    pub(crate) offset: Option<u64>,
    /// None은 Node의 undefined(비교하지 않음)다.
    pub(crate) inode: Option<u64>,
    pub(crate) started_at: String,
}

/// 마지막으로 내보낸 진행 표시. 수집기는 세대 번호로 구별한다.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct PublishedProgress {
    pub(crate) turn_id: String,
    pub(crate) generation: u64,
    pub(crate) revision: u64,
    pub(crate) signature: String,
}

/// 터미널 세션의 바뀌는 값. std Mutex라 await를 건너 쥐지 않는다.
pub(crate) struct SessionState {
    pub(crate) external_session_id: Option<String>,
    pub(crate) running_turn_id: Option<String>,
    pub(crate) last_completed_turn_id: Option<String>,
    pub(crate) claude_cost: ClaudeCost,
    pub(crate) initial_generated_images: HashSet<String>,
    pub(crate) closed: bool,
    pub(crate) closing: bool,
    pub(crate) codex_turn_id: Option<String>,
    pub(crate) activities: TerminalActivityCollector,
    pub(crate) collector_generation: u64,
    pub(crate) structured_result_path: Option<String>,
    pub(crate) dispatch: Option<PendingDispatch>,
    pub(crate) claude_transcript: Option<ClaudeTranscript>,
    /// toolActivityState?.turnID.
    pub(crate) tool_activity_turn: Option<String>,
    pub(crate) tool_events_closing: bool,
    pub(crate) published: Option<PublishedProgress>,
    /// localRelease. 로컬 프로필 직원 터미널이 연 4090 앞문 사용을 닫을 때 돌려준다.
    pub(crate) local_release: Option<EntryRelease>,
}

/// 감시기 커서와 Codex 관문. 세션 turn 잠금 안에서만 바꾼다.
#[derive(Default)]
pub(crate) struct TurnContext {
    pub(crate) antigravity: Option<AntigravityCursor>,
    pub(crate) codex: Option<CodexCursor>,
    pub(crate) codex_gate: Option<CodexTerminalTurnGate>,
}

enum Watcher {
    Antigravity(AntigravityWatcher),
    Codex(CodexWatcher),
}

/// 열린 터미널 하나(Node state).
pub(crate) struct TerminalSession {
    pub(crate) terminal_session_id: String,
    owner_token: String,
    pub(crate) character_id: String,
    pub(crate) character: RuntimeCharacter,
    pub(crate) session_id: String,
    pub(crate) conversation_id: String,
    pub(crate) workdir: String,
    pub(crate) started_at: f64,
    pub(crate) state: Mutex<SessionState>,
    /// 훅 처리·중단·감시기 스윕을 한 줄로 세운다.
    pub(crate) turn: AsyncMutex<TurnContext>,
    /// 개발 도구 이벤트의 활동 기록 상태. 쥐고 있는 동안이 Node activityWritePromise다.
    pub(crate) tool_activity: AsyncMutex<Option<TurnState>>,
    progress: Mutex<Option<ProgressWatcher>>,
    watcher: Mutex<Option<Watcher>>,
}

impl TerminalSession {
    pub(crate) fn lock_state(&self) -> MutexGuard<'_, SessionState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub(crate) fn backend(&self) -> &str {
        &self.character.backend
    }

    /// 진행 감시기를 떼어 내 멈춘다. Node `await state.progressWatcher?.stop(); state.progressWatcher = null`.
    async fn stop_progress(&self) {
        let watcher = self.progress.lock().unwrap_or_else(PoisonError::into_inner).take();
        if let Some(watcher) = watcher {
            watcher.stop().await;
        }
    }

    fn set_progress(&self, watcher: Option<ProgressWatcher>) {
        *self.progress.lock().unwrap_or_else(PoisonError::into_inner) = watcher;
    }

    fn take_watcher(&self) -> Option<Watcher> {
        self.watcher.lock().unwrap_or_else(PoisonError::into_inner).take()
    }
}

#[derive(Default)]
struct Registry {
    sessions: HashMap<String, Arc<TerminalSession>>,
    opening: HashSet<String>,
}

/// 관리자 내부. 감시기 작업도 같은 값을 쥔다.
pub(crate) struct Shared<R> {
    pub(crate) runtime: Arc<R>,
    pub(crate) options: TerminalManagerOptions,
    registry: Mutex<Registry>,
}

/// Node TerminalSessionManager. 공개 메서드는 HTTP 요청이 끊겨도 Node처럼 끝까지 돌도록 별도 작업으로 실행한다.
pub struct TerminalSessionManager<R> {
    shared: Arc<Shared<R>>,
}

fn join_failure(error: tokio::task::JoinError) -> AgentError {
    AgentError::Failed(format!("터미널 작업이 비정상 종료됐습니다. {error}"))
}

fn owner_token() -> String {
    let mut bytes = [0u8; 32];
    // SAFETY: 32바이트 지역 버퍼에 OS 난수를 채운다.
    unsafe { libc::arc4random_buf(bytes.as_mut_ptr().cast(), bytes.len()) };
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

impl<R: TerminalRuntime> TerminalSessionManager<R> {
    pub fn new(runtime: Arc<R>, options: TerminalManagerOptions) -> Self {
        Self { shared: Arc::new(Shared { runtime, options, registry: Mutex::default() }) }
    }

    /// Node `size`. 열린 세션과 여는 중인 세션 수.
    pub fn size(&self) -> usize {
        let registry = self.shared.registry();
        registry.sessions.len() + registry.opening.len()
    }

    pub fn has(&self, character_id: &str) -> bool {
        self.shared.has(character_id)
    }

    pub fn availability(&self, character_id: &str) -> TerminalAvailability {
        self.shared.availability(character_id)
    }

    pub fn list(&self) -> Value {
        self.shared.list()
    }

    pub async fn open(&self, character_id: &str) -> Result<Value, AgentError> {
        let (shared, id) = (Arc::clone(&self.shared), character_id.to_string());
        tokio::spawn(async move { shared.open(&id).await }).await.map_err(join_failure)?
    }

    pub async fn dispatch(&self, request: StartRequest) -> Result<Value, AgentError> {
        Arc::clone(&self.shared).dispatch(request).await
    }

    pub fn dispatch_control(&self, character_id: &str, body: &Value) -> Result<Value, AgentError> {
        self.shared.dispatch_control(character_id, body)
    }

    pub async fn handle_event(&self, character_id: &str, body: Value) -> Result<Value, AgentError> {
        let (shared, id) = (Arc::clone(&self.shared), character_id.to_string());
        tokio::spawn(async move { shared.handle_event(&id, &body).await }).await.map_err(join_failure)?
    }

    pub async fn interrupt(&self, character_id: &str) -> Result<Value, AgentError> {
        let (shared, id) = (Arc::clone(&self.shared), character_id.to_string());
        tokio::spawn(async move { shared.interrupt(&id).await }).await.map_err(join_failure)?
    }

    pub async fn close(&self, character_id: &str) -> Result<bool, AgentError> {
        let (shared, id) = (Arc::clone(&self.shared), character_id.to_string());
        tokio::spawn(async move { shared.close(&id).await }).await.map_err(join_failure)?
    }

    /// 열린 세션을 차례로 닫는다. 하나라도 실패하면 Node처럼 거기서 멈춘다.
    pub async fn shutdown(&self) -> Result<(), AgentError> {
        let ids: Vec<String> = self.shared.registry().sessions.keys().cloned().collect();
        for id in ids {
            self.close(&id).await?;
        }
        Ok(())
    }

    /// recordDeveloperToolEvent의 터미널 분기.
    pub async fn record_tool_event(&self, services: &Arc<RuntimeServices>, event: &DeveloperToolEvent) -> Result<Value, String> {
        let (shared, services, event) = (Arc::clone(&self.shared), Arc::clone(services), event.clone());
        tokio::spawn(async move { shared.record_tool_event(&services, &event).await })
            .await
            .map_err(|error| join_failure(error).to_string())?
    }
}

impl<R: TerminalRuntime> Shared<R> {
    fn registry(&self) -> MutexGuard<'_, Registry> {
        self.registry.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub(crate) fn session(&self, character_id: &str) -> Option<Arc<TerminalSession>> {
        self.registry().sessions.get(character_id).cloned()
    }

    fn has(&self, character_id: &str) -> bool {
        let registry = self.registry();
        registry.sessions.contains_key(character_id) || registry.opening.contains(character_id)
    }

    fn availability(&self, character_id: &str) -> TerminalAvailability {
        let registry = self.registry();
        let mut availability =
            TerminalAvailability { opening: registry.opening.contains(character_id), ..TerminalAvailability::default() };
        if let Some(session) = registry.sessions.get(character_id) {
            let state = session.lock_state();
            availability.running = state.running_turn_id.is_some();
            availability.closing = state.closed || state.closing;
            availability.dispatch = state.dispatch.is_some();
        }
        availability
    }

    fn list(&self) -> Value {
        let sessions: Vec<Arc<TerminalSession>> = self.registry().sessions.values().cloned().collect();
        let mut sessions: Vec<(f64, Value)> = sessions
            .iter()
            .map(|session| {
                let state = session.lock_state();
                let entry = json!({
                    "terminalSessionId": session.terminal_session_id,
                    "characterId": session.character_id,
                    "backend": session.backend(),
                    "externalSessionId": state.external_session_id,
                    "conversationId": session.conversation_id,
                    "runningTurnId": state.running_turn_id,
                    "startedAt": iso_string(session.started_at),
                });
                (session.started_at, entry)
            })
            .collect();
        // Node Map은 넣은 순서다. 여는 시각 순으로 맞춘다.
        sessions.sort_by(|left, right| left.0.total_cmp(&right.0));
        Value::Array(sessions.into_iter().map(|(_, entry)| entry).collect())
    }

    async fn open(self: &Arc<Self>, character_id: &str) -> Result<Value, AgentError> {
        let id = js_trim(character_id).to_string();
        if id.is_empty() {
            return Err(AgentError::CharacterNotFound("캐릭터를 찾을 수 없습니다.".into()));
        }
        {
            let mut registry = self.registry();
            if registry.sessions.contains_key(&id) || registry.opening.contains(&id) {
                return Err(AgentError::Busy("터미널 모드에서 사용 중입니다.".into()));
            }
            registry.opening.insert(id.clone());
        }
        let result = self.open_session(&id).await;
        self.registry().opening.remove(&id);
        result
    }

    async fn open_session(self: &Arc<Self>, id: &str) -> Result<Value, AgentError> {
        let launch = self.runtime.prepare_terminal_launch(id).await?;
        let character = launch.character.clone();
        let backend = character.backend.clone();
        let roots = &self.options.roots;
        let images = list_generated_images(launch.external_session_id.as_deref().unwrap_or(""), &roots.generated_images(&backend))
            .map_err(|error| AgentError::Failed(error.to_string()))?;
        let structured = prepare_structured_turn_result(&launch.workdir, Some(id), &self.options.paths.runtime_id)?;
        let mut context = TurnContext::default();
        if backend == "codex" {
            context.codex_gate = Some(CodexTerminalTurnGate::new(GateOptions {
                cwd: Some(launch.workdir.clone()),
                external_session_id: launch.external_session_id.clone(),
                sessions_root: Some(roots.codex_sessions()),
                ..GateOptions::default()
            }));
            context.codex = Some(CodexCursor::new(roots.codex_sessions()));
        } else if backend == "antigravity" {
            context.antigravity =
                Some(AntigravityCursor::new(&self.options.antigravity_root(), launch.external_session_id.as_deref()));
        }
        let session = Arc::new(TerminalSession {
            terminal_session_id: random_uuid(),
            owner_token: owner_token(),
            character_id: id.to_string(),
            character: character.clone(),
            session_id: launch.session_id.clone(),
            conversation_id: launch.conversation_id.clone(),
            workdir: launch.workdir.clone(),
            started_at: now_milliseconds(),
            state: Mutex::new(SessionState {
                external_session_id: launch.external_session_id.clone(),
                running_turn_id: None,
                last_completed_turn_id: None,
                claude_cost: ClaudeCost::default(),
                initial_generated_images: images.into_iter().collect(),
                closed: false,
                closing: false,
                codex_turn_id: None,
                activities: TerminalActivityCollector::new(Some(launch.workdir.clone())),
                collector_generation: 0,
                structured_result_path: Some(structured),
                dispatch: None,
                claude_transcript: None,
                tool_activity_turn: None,
                tool_events_closing: false,
                published: None,
                local_release: None,
            }),
            turn: AsyncMutex::new(context),
            tool_activity: AsyncMutex::new(None),
            progress: Mutex::new(None),
            watcher: Mutex::new(None),
        });
        self.registry().sessions.insert(id.to_string(), Arc::clone(&session));
        let watcher = match backend.as_str() {
            "antigravity" => Some(Watcher::Antigravity(antigravity::start(self, &session))),
            "codex" => Some(Watcher::Codex(codex::start(self, &session))),
            _ => None,
        };
        *session.watcher.lock().unwrap_or_else(PoisonError::into_inner) = watcher;
        match self.launch_spec(&session, &launch).await {
            Ok(spec) => {
                self.runtime.broadcast(json!({ "type": "terminal.changed", "characterId": id }));
                Ok(spec)
            }
            Err(error) => {
                let path = {
                    let mut state = session.lock_state();
                    state.closed = true;
                    state.structured_result_path.clone()
                };
                match session.take_watcher() {
                    Some(Watcher::Antigravity(watcher)) => {
                        let _ = watcher.stop(self, &session, false).await;
                    }
                    Some(Watcher::Codex(watcher)) => watcher.stop().await,
                    None => {}
                }
                discard_structured_turn_result(path.as_deref());
                self.registry().sessions.remove(id);
                Err(error)
            }
        }
    }

    /// 로컬 프로필 직원은 터미널 환경을 바탕으로 4090 앞문을 열고 실행 파일·인자·환경을 그 명세로 바꾼다.
    /// 앞문을 연 뒤에는 실패할 일이 없어 실패 정리에서 돌려줄 사용이 남지 않는다(launch는 실패하면 스스로 돌려준다).
    async fn launch_spec(&self, session: &TerminalSession, launch: &TerminalLaunch) -> Result<Value, AgentError> {
        let character = &session.character;
        let mut environment = terminal_environment(
            character,
            &self.options.base_environment,
            &launch.workdir,
            &session.character_id,
            self.options.port,
            &self.options.paths,
            &self.options.hook_path,
        );
        let mut executable = locate_executable(&character.backend, &character.config)?;
        let mut arguments = terminal_arguments(
            character,
            launch.external_session_id.as_deref(),
            Some(&launch.workdir),
            &self.options.hook_path,
        )?;
        if let Some(profile) = character.local_profile.as_ref() {
            let Some(providers) = self.runtime.local_providers() else {
                return Err(AgentError::Failed("Local provider service unavailable".into()));
            };
            let definition = normalize_local_definition(profile)?;
            let local = providers
                .launch(
                    character,
                    &definition,
                    LaunchRequest {
                        mode: "terminal",
                        prompt: "",
                        attachments: &[],
                        previous_session_id: launch.external_session_id.as_deref(),
                        workdir: &launch.workdir,
                        base_environment: Some(&environment),
                        hook_path: &self.options.hook_path,
                        executable: &executable,
                        paths: &self.options.paths,
                    },
                )
                .await
                .map_err(|error| AgentError::Failed(error.message))?;
            session.lock_state().local_release = Some(local.release);
            executable = local.launch.executable;
            arguments = local.launch.args;
            environment = local.launch.env;
        }
        Ok(json!({
            "terminalSessionId": session.terminal_session_id,
            "ownerToken": session.owner_token,
            "executable": executable,
            "args": arguments,
            "cwd": launch.workdir,
            "env": environment,
            "externalSessionId": launch.external_session_id,
            "conversationId": launch.conversation_id,
            "backend": character.backend,
        }))
    }

    /// bindIfNeeded. CLI가 알려 준 세션 ID가 새것이면 직원의 활성 CLI 세션으로 묶는다.
    pub(crate) async fn bind_if_needed(&self, session: &TerminalSession, external_session_id: &str) -> Result<(), AgentError> {
        let id = js_trim(external_session_id);
        if id.is_empty() || session.lock_state().external_session_id.as_deref() == Some(id) {
            return Ok(());
        }
        self.runtime.bind_terminal_external_session(&session.character_id, &session.session_id, id).await?;
        session.lock_state().external_session_id = Some(id.to_string());
        Ok(())
    }

    /// resetTerminalArtifacts. 다음 턴을 위해 활동 수집기·생성 이미지 기준·구조화 결과 파일을 새로 만든다.
    pub(crate) fn reset(&self, session: &TerminalSession) -> Result<(), AgentError> {
        let external = {
            let mut state = session.lock_state();
            state.tool_activity_turn = None;
            state.tool_events_closing = false;
            state.activities = TerminalActivityCollector::new(Some(session.workdir.clone()));
            state.collector_generation += 1;
            state.external_session_id.clone().unwrap_or_default()
        };
        let images = list_generated_images(&external, &self.options.roots.generated_images(session.backend()))
            .map_err(|error| AgentError::Failed(error.to_string()))?;
        session.lock_state().initial_generated_images = images.into_iter().collect();
        let path = prepare_structured_turn_result(&session.workdir, Some(&session.character_id), &self.options.paths.runtime_id)?;
        session.lock_state().structured_result_path = Some(path);
        Ok(())
    }

    /// 터미널 턴 완료. 개발 도구 이벤트가 이 턴에 쓰고 있으면 받기를 닫고 쓰기가 끝나길 기다린 뒤 앞 활동을 되읽게 한다.
    pub(crate) async fn complete_turn(
        &self,
        session: &TerminalSession,
        mut completion: TerminalTurnCompletion,
    ) -> Result<(), AgentError> {
        let matched = {
            let mut state = session.lock_state();
            let matched = state.tool_activity_turn.as_deref() == Some(completion.turn_id.as_str());
            if matched {
                state.tool_events_closing = true;
            }
            matched
        };
        if matched {
            drop(session.tool_activity.lock().await);
        }
        completion.restore_tool_activities = matched;
        self.runtime.complete_terminal_turn(completion).await
    }

    pub(crate) fn changed(&self, session: &TerminalSession) {
        self.runtime.broadcast(json!({ "type": "terminal.changed", "characterId": session.character_id }));
    }

    async fn handle_event(self: &Arc<Self>, character_id: &str, body: &Value) -> Result<Value, AgentError> {
        let Some(session) = self.session(character_id).filter(|session| !session.lock_state().closed) else {
            return Err(AgentError::Failed(SESSION_NOT_OPEN.into()));
        };
        let source = crate::js::js_string_or_empty(body.get("source"));
        let empty = json!({});
        let payload = body.get("payload").filter(|payload| !payload.is_null()).unwrap_or(&empty);
        let mut context = session.turn.lock().await;
        match (source.as_str(), session.backend()) {
            ("claude", "claude") => self.claude_event(&session, payload).await,
            ("codex", "codex") => self.codex_event(&session, &mut context, payload).await,
            _ => Err(AgentError::Failed("터미널 CLI와 이벤트 종류가 일치하지 않습니다.".into())),
        }
    }

    /// interrupt. 앱이 터미널에 Esc를 보낸 뒤 부른다. Claude는 중단 때 Stop 훅이 오지 않아 여기서 턴을 닫는다.
    async fn interrupt(self: &Arc<Self>, character_id: &str) -> Result<Value, AgentError> {
        let Some(session) = self.session(character_id).filter(|session| !session.lock_state().closed) else {
            return Err(AgentError::Failed(SESSION_NOT_OPEN.into()));
        };
        dispatch::cancel(&mut session.lock_state());
        let mut context = session.turn.lock().await;
        let Some(turn_id) = session.lock_state().running_turn_id.clone() else {
            return Ok(json!({ "interrupted": false, "turnId": null }));
        };
        session.stop_progress().await;
        self.runtime.interrupt_terminal_turn(character_id, &turn_id).await?;
        {
            let mut state = session.lock_state();
            state.running_turn_id = None;
            state.codex_turn_id = None;
        }
        if session.backend() == "antigravity"
            && let Some(cursor) = context.antigravity.as_mut()
        {
            cursor.forget_current_turn();
        }
        self.reset(&session)?;
        self.changed(&session);
        Ok(json!({ "interrupted": true, "turnId": turn_id }))
    }

    async fn close(self: &Arc<Self>, character_id: &str) -> Result<bool, AgentError> {
        let Some(session) = self.session(character_id) else {
            return Ok(false);
        };
        session.lock_state().closing = true;
        session.stop_progress().await;
        dispatch::cancel(&mut session.lock_state());
        match session.take_watcher() {
            Some(Watcher::Antigravity(watcher)) => watcher.stop(self, &session, true).await?,
            Some(Watcher::Codex(watcher)) => watcher.stop().await,
            None => {}
        }
        let (path, release) = {
            let mut state = session.lock_state();
            state.closed = true;
            (state.structured_result_path.clone(), state.local_release.clone())
        };
        if let Some(release) = release {
            release.release().await.map_err(|error| AgentError::Failed(error.message))?;
        }
        discard_structured_turn_result(path.as_deref());
        let _context = session.turn.lock().await;
        let running = session.lock_state().running_turn_id.clone();
        if let Some(turn_id) = running {
            self.runtime.interrupt_terminal_turn(character_id, &turn_id).await?;
            session.lock_state().running_turn_id = None;
        }
        self.registry().sessions.remove(character_id);
        self.changed(&session);
        Ok(true)
    }

    async fn record_tool_event(&self, services: &RuntimeServices, event: &DeveloperToolEvent) -> Result<Value, String> {
        let inactive = json!({ "accepted": false, "reason": "inactive_session" });
        let Some(session) = self.session(&event.character_id) else {
            return Ok(inactive);
        };
        let accepting = |state: &SessionState| {
            !state.closed
                && !state.tool_events_closing
                && state.external_session_id.as_deref() == Some(event.session_id.as_str())
        };
        {
            let state = session.lock_state();
            if !accepting(&state) || state.running_turn_id.is_none() {
                return Ok(inactive);
            }
        }
        let mut tool = session.tool_activity.lock().await;
        // 기다리는 동안 완료가 받기를 닫았을 수 있어 다시 본다.
        let running = {
            let mut state = session.lock_state();
            let Some(running) = state.running_turn_id.clone().filter(|_| accepting(&state)) else {
                return Ok(inactive);
            };
            let stale = state.tool_activity_turn.as_deref() != Some(running.as_str())
                || tool.as_ref().is_none_or(|current| current.turn_id != running);
            if stale {
                state.tool_activity_turn = Some(running.clone());
            }
            stale.then_some(running)
        };
        if let Some(turn_id) = running {
            *tool = Some(tool_activity_state(&turn_id, &event.character_id));
        }
        let Some(state) = tool.as_mut() else {
            return Ok(inactive);
        };
        if !state.developer_tool_event_keys.admit(&event.event_key) {
            return Ok(json!({ "accepted": false, "reason": "event_limit" }));
        }
        state.add_activity(services, &event.activity()).await?;
        Ok(json!({ "accepted": true }))
    }
}

/// Node toolActivityState. 활동 기록에 필요한 턴 ID와 직원 ID만 채운다.
fn tool_activity_state(turn_id: &str, character_id: &str) -> TurnState {
    TurnState::new(TurnSeed {
        turn_id: turn_id.to_string(),
        session_id: String::new(),
        conversation_id: String::new(),
        external_session_id: None,
        resume_execution_workdir: None,
        character: RuntimeCharacter {
            id: Some(character_id.to_string()),
            name: String::new(),
            seat: String::new(),
            backend: String::new(),
            model: None,
            effort: String::new(),
            fast_mode: false,
            auto_compact_percent: None,
            permission: String::new(),
            identity_prompt: String::new(),
            config: Value::Null,
            local_profile: None,
        },
        prompt: String::new(),
        execution_prompt: String::new(),
        workdir: String::new(),
        structured_result_path: None,
        attachments: Vec::new(),
        reply_recipient_ids: Vec::new(),
    })
}
