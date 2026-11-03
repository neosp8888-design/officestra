// 터미널 관리자 단위 시험. DB 없는 가짜 실행기로 Node terminal-sessions·terminal-live-progress 시험을 옮긴다.
//
// Node 시험은 감시기의 sweep()을 직접 불렀다. 여기서는 같은 일을 하는 sweep_locked·Cursor::sweep을 부르고,
// 직접 부르는 시험에서는 배경 감시를 멈추거나 주기를 길게 둬 같은 줄을 두 곳이 먼저 읽지 않게 한다.

use std::collections::{HashSet, VecDeque};
use std::fs::{self, OpenOptions};
use std::io::Write as _;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tokio::sync::Notify;
use tokio::task::JoinHandle;

use super::launch::{StepRow, TerminalStep, parse_antigravity_terminal_step, terminal_arguments};
use super::progress::{Cursor, ProgressWatcher};
use super::{
    ClaudeTranscript, Shared, TerminalLaunch, TerminalManagerOptions, TerminalRuntime, TerminalSession,
    TerminalSessionManager, TerminalTurnCompletion, TerminalTurnStart, Watcher, antigravity, codex, progress,
};
use crate::agent_runtime::launch::{LaunchPaths, RuntimeCharacter};
use crate::agent_runtime::lifecycle::StartRequest;
use crate::agent_runtime::service::{AgentError, SessionRoots};
use crate::antigravity_local_state::SqliteValue;
use crate::js::{join_path, random_uuid};
use crate::agent_runtime::locate_executable;
use crate::local_embedding::Environment;
use crate::local_provider::normalize_local_definition;
use crate::local_provider::service::LocalProviderService;
use crate::local_provider::service::tests::{Options, character_for, definition_value as local_definition_value, harness};
use crate::structured_turn_result::{
    STRUCTURED_RESULT_ENV, STRUCTURED_RESULT_GUIDANCE, structured_turn_result_path, submit_structured_response_source,
};
use crate::terminal_turn_activities::TerminalActivityCollector;

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// mkdtemp. macOS 임시 폴더는 /var → /private/var 링크라 실제 경로를 쓴다.
struct TempDirectory(String);

impl TempDirectory {
    fn new(prefix: &str) -> Self {
        let path = std::env::temp_dir().join(format!("{prefix}-{}", random_uuid()));
        fs::create_dir_all(&path).unwrap();
        Self(fs::canonicalize(&path).unwrap().to_string_lossy().into_owned())
    }

    fn join(&self, parts: &[&str]) -> String {
        let mut all = vec![self.0.as_str()];
        all.extend_from_slice(parts);
        join_path(&all)
    }
}

impl Drop for TempDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn append(path: &str, text: &str) {
    append_bytes(path, text.as_bytes());
}

fn append_bytes(path: &str, bytes: &[u8]) {
    let mut file = OpenOptions::new().create(true).append(true).open(path).unwrap();
    file.write_all(bytes).unwrap();
}

fn lines(records: &[Value]) -> String {
    records.iter().map(|record| format!("{record}\n")).collect()
}

async fn wait_until(mut predicate: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        if predicate() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("조건이 제한 시간 안에 충족되지 않았습니다.");
}

fn character(id: &str, backend: &str, permission: &str) -> RuntimeCharacter {
    RuntimeCharacter {
        id: Some(id.into()),
        name: String::new(),
        seat: String::new(),
        backend: backend.into(),
        model: Some("model-a".into()),
        effort: "high".into(),
        fast_mode: false,
        auto_compact_percent: None,
        permission: permission.into(),
        identity_prompt: "업무 지침".into(),
        config: json!({}),
        local_profile: None,
    }
}

/// fakeRuntime의 prepareTerminalLaunch 값.
#[derive(Clone)]
struct Setup {
    backend: &'static str,
    external_session_id: Option<String>,
    workdir: String,
    permission: &'static str,
    /// 있으면 character() 대신 이 직원으로 연다(로컬 프로필 직원 시험).
    character: Option<RuntimeCharacter>,
    local_providers: Option<LocalProviderService>,
}

impl Setup {
    fn backend(backend: &'static str) -> Self {
        let permission = if backend == "antigravity" { "dangerously-skip-permissions" } else { "danger-full-access" };
        Self { backend, external_session_id: None, workdir: "/tmp/office".into(), permission, character: None, local_providers: None }
    }
}

#[derive(Default)]
struct Calls {
    begun: Vec<TerminalTurnStart>,
    completed: Vec<TerminalTurnCompletion>,
    /// terminal.work-status의 {turnID, ...workActivity}.
    progress: Vec<Value>,
    events: Vec<Value>,
    interrupted: Vec<(String, String)>,
    bound: Vec<String>,
    costs: Vec<(String, String, f64)>,
    tag_queries: Vec<String>,
}

impl Calls {
    fn begun_prompts(&self) -> Vec<&str> {
        self.begun.iter().map(|start| start.prompt.as_str()).collect()
    }

    fn has_event(&self, kind: &str) -> bool {
        self.events.iter().any(|event| event["type"] == kind)
    }
}

/// Node fakeRuntime. 턴 ID는 `turn-<시작 순번>`이다.
struct FakeRuntime {
    setup: Setup,
    calls: Mutex<Calls>,
    /// 다음 완료 저장에서 차례로 낼 오류.
    complete_errors: Mutex<VecDeque<String>>,
    dispatch_busy: AtomicBool,
    compacting: Mutex<HashSet<String>>,
    tags: Mutex<Vec<String>>,
    hold_tags: AtomicBool,
    release_tags: Notify,
}

impl FakeRuntime {
    fn calls(&self) -> MutexGuard<'_, Calls> {
        lock(&self.calls)
    }
}

impl TerminalRuntime for FakeRuntime {
    fn is_draining(&self) -> bool {
        false
    }

    fn character_busy(&self, character_id: &str) -> bool {
        self.is_compacting(character_id)
    }

    fn is_compacting(&self, character_id: &str) -> bool {
        lock(&self.compacting).contains(character_id)
    }

    fn broadcast(&self, event: Value) {
        let mut calls = self.calls();
        if event["type"] == "terminal.work-status" {
            let mut entry = serde_json::Map::new();
            entry.insert("turnID".into(), event["turnId"].clone());
            if let Some(work) = event["workActivity"].as_object() {
                entry.extend(work.clone());
            }
            calls.progress.push(Value::Object(entry));
        }
        calls.events.push(event);
    }

    async fn prepare_terminal_launch(&self, character_id: &str) -> Result<TerminalLaunch, AgentError> {
        Ok(TerminalLaunch {
            character: self.setup.character.clone().unwrap_or_else(|| character(character_id, self.setup.backend, self.setup.permission)),
            session_id: "db-session".into(),
            conversation_id: "conversation".into(),
            external_session_id: self.setup.external_session_id.clone(),
            workdir: self.setup.workdir.clone(),
        })
    }

    async fn bind_terminal_external_session(&self, _: &str, _: &str, external_session_id: &str) -> Result<(), AgentError> {
        self.calls().bound.push(external_session_id.into());
        Ok(())
    }

    async fn begin_terminal_turn(&self, start: TerminalTurnStart) -> Result<String, AgentError> {
        let mut calls = self.calls();
        calls.begun.push(start);
        Ok(format!("turn-{}", calls.begun.len()))
    }

    async fn complete_terminal_turn(&self, completion: TerminalTurnCompletion) -> Result<(), AgentError> {
        let failure = lock(&self.complete_errors).pop_front();
        if let Some(message) = failure {
            return Err(AgentError::Failed(message));
        }
        self.calls().completed.push(completion);
        Ok(())
    }

    async fn add_terminal_turn_reported_cost(&self, character_id: &str, turn_id: &str, cost_usd: f64) -> Result<bool, AgentError> {
        self.calls().costs.push((character_id.into(), turn_id.into(), cost_usd));
        Ok(true)
    }

    async fn interrupt_terminal_turn(&self, character_id: &str, turn_id: &str) -> Result<bool, AgentError> {
        self.calls().interrupted.push((character_id.into(), turn_id.into()));
        Ok(true)
    }

    async fn terminal_dispatch_busy(&self, _: &str) -> Result<bool, AgentError> {
        Ok(self.dispatch_busy.load(Ordering::SeqCst))
    }

    async fn reply_route_tags(&self, character_id: &str) -> Result<Vec<String>, AgentError> {
        self.calls().tag_queries.push(character_id.into());
        if self.hold_tags.load(Ordering::SeqCst) {
            self.release_tags.notified().await;
        }
        Ok(lock(&self.tags).clone())
    }

    fn local_providers(&self) -> Option<LocalProviderService> {
        self.setup.local_providers.clone()
    }
}

fn manager_options(home: &str, runtime_id: &str) -> TerminalManagerOptions {
    TerminalManagerOptions {
        antigravity_poll_interval: Duration::from_secs(60),
        codex_poll_interval: Duration::from_secs(60),
        ..TerminalManagerOptions::new(
            SessionRoots { home: home.into() },
            LaunchPaths {
                result_tool_directory: "/tmp/officestra-result-tool".into(),
                node_executable: Some("/usr/bin/node".into()),
                playwright_driver_directory: "/tmp/officestra-playwright".into(),
                runtime_id: runtime_id.into(),
            },
            Environment::from([("PATH".to_string(), "/usr/bin".to_string()), ("LANG".to_string(), "ko_KR.UTF-8".to_string())]),
            "/tmp/officestra-hook/officestra-terminal-hook".into(),
        )
    }
}

/// 가짜 실행기를 붙인 관리자. 홈 폴더는 시험마다 새로 만든다.
struct Fixture {
    runtime: Arc<FakeRuntime>,
    manager: TerminalSessionManager<FakeRuntime>,
    runtime_id: String,
    home: TempDirectory,
}

impl Fixture {
    fn new(setup: Setup) -> Self {
        Self::build(|_| setup, |_| {})
    }

    fn build(setup: impl FnOnce(&TempDirectory) -> Setup, adjust: impl FnOnce(&mut TerminalManagerOptions)) -> Self {
        let home = TempDirectory::new("officestra-terminal");
        let runtime_id = format!("terminal-test-{}", random_uuid());
        let mut options = manager_options(&home.0, &runtime_id);
        adjust(&mut options);
        let runtime = Arc::new(FakeRuntime {
            setup: setup(&home),
            calls: Mutex::default(),
            complete_errors: Mutex::default(),
            dispatch_busy: AtomicBool::new(false),
            compacting: Mutex::default(),
            tags: Mutex::default(),
            hold_tags: AtomicBool::new(false),
            release_tags: Notify::new(),
        });
        let manager = TerminalSessionManager::new(Arc::clone(&runtime), options);
        Self { runtime, manager, runtime_id, home }
    }

    fn shared(&self) -> &Arc<Shared<FakeRuntime>> {
        &self.manager.shared
    }

    fn session(&self) -> Arc<TerminalSession> {
        self.shared().session("boss").expect("열린 터미널 세션")
    }

    fn calls(&self) -> MutexGuard<'_, Calls> {
        self.runtime.calls()
    }

    async fn claude(&self, payload: Value) -> Result<Value, AgentError> {
        self.manager.handle_event("boss", json!({ "source": "claude", "payload": payload })).await
    }

    async fn prompt(&self, text: &str) -> Value {
        self.claude(json!({ "hook_event_name": "UserPromptSubmit", "session_id": "claude-session", "prompt": text }))
            .await
            .unwrap()
    }

    async fn stop(&self, text: &str) -> Result<Value, AgentError> {
        self.claude(json!({ "hook_event_name": "Stop", "session_id": "claude-session", "last_assistant_message": text })).await
    }

    fn running(&self) -> Option<String> {
        self.session().lock_state().running_turn_id.clone()
    }

    fn structured_path(&self) -> String {
        self.session().lock_state().structured_result_path.clone().expect("구조화 결과 경로")
    }

    /// 배경 감시를 멈춘다. Node 시험이 fs.watch와 주기 타이머를 끄던 것과 같다.
    async fn stop_watcher(&self) {
        let session = self.session();
        match session.take_watcher() {
            Some(Watcher::Antigravity(watcher)) => watcher.stop(self.shared(), &session, false).await.unwrap(),
            Some(Watcher::Codex(watcher)) => watcher.stop().await,
            None => {}
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        // 구조화 결과 파일은 시험마다 다른 runtime_id 폴더 아래에 있다.
        let path = structured_turn_result_path("/tmp/office", Some("boss"), &self.runtime_id);
        if let Some(root) = Path::new(&path).parent().and_then(Path::parent) {
            let _ = fs::remove_dir_all(root);
        }
    }
}

fn varint(mut value: u64) -> Vec<u8> {
    let mut bytes = Vec::new();
    loop {
        let byte = (value & 0x7f) as u8;
        value >>= 7;
        if value == 0 {
            bytes.push(byte);
            return bytes;
        }
        bytes.push(byte | 0x80);
    }
}

fn number_field(number: u64, value: u64) -> Vec<u8> {
    [varint(number << 3), varint(value)].concat()
}

fn bytes_field(number: u64, value: &[u8]) -> Vec<u8> {
    [varint((number << 3) | 2), varint(value.len() as u64), value.to_vec()].concat()
}

fn message(fields: &[Vec<u8>]) -> Vec<u8> {
    fields.concat()
}

fn value_after<'a>(arguments: &'a [String], flag: &str) -> &'a str {
    let index = arguments.iter().position(|argument| argument == flag).unwrap_or_else(|| panic!("{flag} 인자가 없습니다."));
    &arguments[index + 1]
}

fn has(arguments: &[String], value: &str) -> bool {
    arguments.iter().any(|argument| argument == value)
}

const HOOK: &str = "/tmp/hook";

#[test]
fn three_cli_arguments_keep_resume_and_permission() {
    for permission in ["read-only", "workspace-write", "danger-full-access"] {
        let character = character("boss", "codex", permission);
        let fresh = terminal_arguments(&character, None, Some("/tmp/office"), HOOK).unwrap();
        let resumed = terminal_arguments(&character, Some("session"), Some("/tmp/office"), HOOK).unwrap();
        assert!(!has(&fresh, "resume"));
        assert_eq!(resumed[..2], ["resume", "session"]);
        assert_eq!(value_after(&fresh, "-s"), permission);
        // Node는 [node, 훅 스크립트]였고 Rust는 훅 실행 파일 하나다.
        assert!(has(&fresh, r#"notify=["/tmp/hook"]"#));
    }
    for permission in ["plan", "accept-edits", "dangerously-skip-permissions"] {
        let character = character("boss", "antigravity", permission);
        let fresh = terminal_arguments(&character, None, Some("/tmp/office"), HOOK).unwrap();
        let resumed = terminal_arguments(&character, Some("session"), Some("/tmp/office"), HOOK).unwrap();
        assert!(!has(&fresh, "--conversation"));
        assert_eq!(value_after(&resumed, "--conversation"), "session");
        assert_eq!(value_after(&fresh, "--add-dir"), "/tmp/office");
    }
    for permission in ["plan", "acceptEdits", "bypassPermissions"] {
        let character = character("boss", "claude", permission);
        let fresh = terminal_arguments(&character, None, Some("/tmp/office"), HOOK).unwrap();
        let resumed = terminal_arguments(&character, Some("session"), Some("/tmp/office"), HOOK).unwrap();
        assert!(!has(&fresh, "--resume"));
        assert_eq!(value_after(&resumed, "--resume"), "session");
        assert_eq!(value_after(&fresh, "--permission-mode"), permission);
        let settings: Value = serde_json::from_str(value_after(&fresh, "--settings")).unwrap();
        assert!(settings["hooks"]["UserPromptSubmit"].is_array());
        assert!(settings["hooks"]["Stop"].is_array());
        assert!(value_after(&fresh, "--append-system-prompt").contains(STRUCTURED_RESULT_GUIDANCE));
    }
}

#[test]
fn terminal_passes_discovered_model_and_effort_through() {
    let mut codex = character("boss", "codex", "danger-full-access");
    codex.model = Some("gpt-future-mini".into());
    codex.effort = "medium".into();
    let arguments = terminal_arguments(&codex, None, Some("/tmp/office"), HOOK).unwrap();
    assert!(has(&arguments, r#"model="gpt-future-mini""#));
    assert!(has(&arguments, r#"model_reasoning_effort="medium""#));

    let mut antigravity = character("boss", "antigravity", "accept-edits");
    antigravity.model = Some("gemini-future-pro".into());
    antigravity.effort = "low".into();
    let arguments = terminal_arguments(&antigravity, None, Some("/tmp/office"), HOOK).unwrap();
    assert_eq!(value_after(&arguments, "--model"), "gemini-future-pro-low");
    assert!(!has(&arguments, "--effort"));
}

#[test]
fn antigravity_thinking_terminal_sends_no_effort_for_fixed_model() {
    for effort in ["high", "default"] {
        let mut character = character("boss", "antigravity", "plan");
        character.model = Some("claude-opus-4-6-thinking".into());
        character.effort = effort.into();
        let arguments = terminal_arguments(&character, Some("opus-session"), Some("/tmp/office"), HOOK).unwrap();
        assert_eq!(value_after(&arguments, "--model"), "claude-opus-4-6-thinking");
        assert!(!has(&arguments, "--effort"));
        assert_eq!(value_after(&arguments, "--conversation"), "opus-session");
    }
}

#[test]
fn claude_settings_status_line_uses_hook_command() {
    let arguments = terminal_arguments(&character("boss", "claude", "plan"), None, Some("/tmp/office"), HOOK).unwrap();
    let settings: Value = serde_json::from_str(arguments.iter().find(|value| value.starts_with('{')).unwrap()).unwrap();
    assert_eq!(settings["statusLine"], json!({ "type": "command", "command": "officestra-terminal-hook" }));
}

fn number(value: &Value) -> f64 {
    value.as_f64().unwrap_or(f64::NAN)
}

fn with(mut base: Value, extra: Value) -> Value {
    if let (Some(base), Value::Object(extra)) = (base.as_object_mut(), extra) {
        base.extend(extra);
    }
    base
}

#[tokio::test]
async fn open_terminal_blocks_second_process_and_reports_running_turn() {
    let f = Fixture::new(Setup::backend("claude"));
    f.manager.open("boss").await.unwrap();
    assert!(matches!(f.manager.open("boss").await, Err(AgentError::Busy(_))));
    assert!(f.manager.has("boss"));
    assert_eq!(f.manager.size(), 1);
    // 실행기는 이 값으로 같은 직원의 GUI 업무를 409로 막는다. HTTP 경로는 DB 통합 시험에서 본다.
    f.session().lock_state().running_turn_id = Some("busy-turn".into());
    assert!(f.manager.availability("boss").running);
    assert_eq!(f.manager.list()[0]["characterId"], "boss");
    assert_eq!(f.calls().events.last().unwrap()["type"], "terminal.changed");
    f.manager.close("boss").await.unwrap();
    assert!(!f.manager.has("boss"));
}

/// 로컬 프로필 직원(Node character.localProfile). Node 터미널 시험에는 없는 경로라 Rust에서 따로 본다.
fn local_setup(providers: Option<LocalProviderService>) -> Setup {
    let value = local_definition_value();
    let mut local = character_for(&normalize_local_definition(&value).unwrap());
    local.id = Some("boss".into());
    local.local_profile = Some(value);
    Setup { character: Some(local), local_providers: providers, ..Setup::backend("claude") }
}

#[tokio::test]
async fn local_terminal_without_provider_service_cleans_up_half_open_session() {
    let f = Fixture::new(local_setup(None));
    let error = f.manager.open("boss").await.err().expect("로컬 서비스 없이 열리면 안 된다");
    assert_eq!(error, AgentError::Failed("Local provider service unavailable".into()));
    assert!(!f.manager.has("boss"));
    assert!(!f.calls().has_event("terminal.changed"));
    let path = structured_turn_result_path("/tmp/office", Some("boss"), &f.runtime_id);
    assert!(!Path::new(&path).exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn local_terminal_launches_on_terminal_environment_and_releases_on_close() {
    let service = harness(Options::default());
    let f = Fixture::new(local_setup(Some(service.service.clone())));
    let spec = f.manager.open("boss").await.unwrap();
    // 터미널 환경(기본 환경·훅 PATH·구조화 결과) 위에 앞문 주소를 얹는다. 정의의 endpoint는 앞문으로 바뀐다.
    let env = &spec["env"];
    assert_eq!(env["LANG"], "ko_KR.UTF-8");
    assert!(env["PATH"].as_str().unwrap().split(':').any(|part| part == "/tmp/officestra-hook"));
    assert_eq!(env[STRUCTURED_RESULT_ENV].as_str(), Some(f.structured_path().as_str()));
    let base_url = env["ANTHROPIC_BASE_URL"].as_str().unwrap();
    assert!(base_url.starts_with("http://127.0.0.1:") && base_url != "http://127.0.0.1:41235");
    assert_eq!(spec["executable"], locate_executable("claude", &json!({})).unwrap());
    let args: Vec<&str> = spec["args"].as_array().unwrap().iter().filter_map(Value::as_str).collect();
    let settings = args.iter().position(|argument| *argument == "--settings").map(|index| args[index + 1]).unwrap();
    assert!(settings.contains("officestra-terminal-hook"));
    assert!(!args.contains(&"-p"));
    assert_eq!(service.service.status().len(), 1);
    f.manager.close("boss").await.unwrap();
    assert!(!f.manager.has("boss"));
    assert!(service.service.status().is_empty());
    service.finish().await;
}

#[tokio::test]
async fn claude_prompt_and_stop_hooks_create_and_complete_same_turn() {
    let f = Fixture::new(Setup::backend("claude"));
    f.manager.open("boss").await.unwrap();
    let started = f.prompt("터미널 질문").await;
    assert_eq!(started["accepted"], true);
    let completed = f.stop("터미널 답변").await.unwrap();
    assert_eq!(completed["turnId"], started["turnId"]);
    {
        let calls = f.calls();
        assert_eq!(calls.begun_prompts(), ["터미널 질문"]);
        assert_eq!(calls.completed.iter().map(|turn| turn.response.as_str()).collect::<Vec<_>>(), ["터미널 답변"]);
        assert_eq!(calls.bound, ["claude-session"]);
    }
    f.manager.close("boss").await.unwrap();
}

#[tokio::test]
async fn claude_terminal_streams_new_transcript_and_stops_at_turn_boundary() {
    let root = TempDirectory::new("claude-terminal-live");
    let path = root.join(&["new-session.jsonl"]);
    let f = Fixture::new(Setup { workdir: root.0.clone(), ..Setup::backend("claude") });
    f.manager.open("boss").await.unwrap();
    let payload = json!({ "session_id": "claude-session", "transcript_path": path });
    f.claude(with(payload.clone(), json!({ "hook_event_name": "UserPromptSubmit", "prompt": "질문" }))).await.unwrap();
    let session = f.session();
    assert!(lock(&session.progress).is_some());
    let first = json!({ "type": "assistant", "sessionId": "claude-session", "message": {
        "id": "progress",
        "content": [{ "type": "text", "text": "파일 확인 중" },
            { "type": "tool_use", "id": "read-1", "name": "Read", "input": { "file_path": "/repo/file.txt" } }],
    } });
    fs::write(&path, lines(&[first])).unwrap();
    wait_until(|| !f.calls().progress.is_empty()).await;
    // 늦게 오는 두 번째 파일 알림이 같은 상태를 다시 보내지 않는지 잠시 더 본다.
    tokio::time::sleep(Duration::from_millis(300)).await;
    {
        let calls = f.calls();
        assert!(calls.completed.is_empty());
        assert_eq!(calls.progress.len(), 1);
        assert!(calls.progress.iter().any(|activity| activity["kind"] == "tool" && activity["status"] == "running"));
    }
    append(&path, &lines(&[
        json!({ "type": "user", "sessionId": "claude-session", "message": {
            "content": [{ "type": "tool_result", "tool_use_id": "read-1", "content": "private output" }] } }),
        json!({ "type": "assistant", "sessionId": "claude-session", "message": {
            "id": "final", "usage": { "input_tokens": 3, "output_tokens": 2 }, "content": [{ "type": "text", "text": "완료" }] } }),
    ]));
    let expected = json!({ "turnID": "turn-1", "kind": "message", "text": "", "status": "completed" });
    wait_until(|| f.calls().progress.last() == Some(&expected)).await;
    assert!(!f.calls().progress.iter().any(|activity| activity["text"].as_str().unwrap_or("").contains("private output")));
    f.claude(with(payload, json!({ "hook_event_name": "Stop", "last_assistant_message": "완료" }))).await.unwrap();
    assert!(lock(&session.progress).is_none());
    {
        let calls = f.calls();
        assert_eq!(calls.completed[0].response, "완료");
        assert!(!calls.completed[0].activities.iter().any(|activity| activity["text"] == "완료"));
    }
    let count = f.calls().progress.len();
    append(&path, &lines(&[json!({ "type": "assistant", "message": { "content": [{ "type": "text", "text": "종료 이후" }] } })]));
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(f.calls().progress.len(), count);
    f.manager.close("boss").await.unwrap();
}

#[tokio::test]
async fn claude_activities_after_start_and_submitted_sources_reach_same_completion() {
    let root = TempDirectory::new("claude-terminal-history");
    let path = root.join(&["session.jsonl"]);
    fs::write(&path, lines(&[json!({ "type": "assistant", "sessionId": "claude-session",
        "message": { "content": [{ "type": "text", "text": "이전 답변" }] } })]))
    .unwrap();
    let f = Fixture::new(Setup { workdir: root.0.clone(), ..Setup::backend("claude") });
    let spec = f.manager.open("boss").await.unwrap();
    let result_path = spec["env"][STRUCTURED_RESULT_ENV].as_str().unwrap().to_string();
    assert_eq!(result_path, f.structured_path());
    let hook = |event: &str, extra: Value| {
        with(json!({ "hook_event_name": event, "session_id": "claude-session", "transcript_path": path }), extra)
    };
    f.claude(hook("UserPromptSubmit", json!({ "prompt": "질문" }))).await.unwrap();
    append(&path, &lines(&[json!({ "type": "assistant", "sessionId": "claude-session", "message": {
        "id": "one", "model": "claude-opus-5-5", "usage": { "input_tokens": 30, "output_tokens": 4 },
        "content": [{ "type": "text", "text": "확인 중" }, { "type": "thinking", "thinking": "공개 요약" }] } })]));
    submit_structured_response_source(&result_path, json!({ "kind": "file", "title": "근거", "locator": "source.mjs" })).unwrap();
    f.claude(hook("Stop", json!({ "last_assistant_message": "완료" }))).await.unwrap();
    {
        let calls = f.calls();
        let first = &calls.completed[0];
        let mut kinds: Vec<&str> = first.activities.iter().filter_map(|activity| activity["kind"].as_str()).collect();
        kinds.sort_unstable();
        assert_eq!(kinds, ["message", "thinking"]);
        assert_eq!(first.structured.as_ref().unwrap().sources[0]["locator"], "source.mjs");
        assert_eq!(first.response, "완료");
        let usage = first.usage.as_ref().unwrap();
        assert_eq!(number(&usage["inputTokens"]), 30.0);
        assert_eq!(number(&usage["outputTokens"]), 4.0);
        assert_eq!(first.resolved_model.as_deref(), Some("claude-opus-5-5"));
    }
    f.claude(hook("UserPromptSubmit", json!({ "prompt": "다음 질문" }))).await.unwrap();
    f.claude(hook("Stop", json!({ "last_assistant_message": "다음 답변" }))).await.unwrap();
    {
        let calls = f.calls();
        assert!(calls.completed[1].activities.is_empty());
        assert!(calls.completed[1].structured.as_ref().unwrap().sources.is_empty());
    }
    f.manager.close("boss").await.unwrap();
}

// Claude Code 상태줄은 세션 누적 비용만 알려 주고, 턴 마지막 갱신은 Stop 훅보다 늦게 온다.
#[tokio::test]
async fn claude_status_line_cost_splits_per_turn_and_late_update_goes_to_previous_turn() {
    let f = Fixture::new(Setup::backend("claude"));
    f.manager.open("boss").await.unwrap();
    let status = |total: f64| {
        f.claude(json!({ "session_id": "claude-session", "cost": { "total_cost_usd": total, "total_duration_ms": 10 } }))
    };
    status(0.0).await.unwrap();
    f.prompt("첫 질문").await;
    status(0.1).await.unwrap();
    f.stop("답변").await.unwrap();
    assert_eq!(f.calls().completed[0].reported_cost_usd, Some(0.1));
    // 턴이 끝난 뒤 도착한 마지막 갱신은 직전 턴에 차액만 더한다.
    status(0.25).await.unwrap();
    status(0.25).await.unwrap();
    assert_eq!(f.calls().costs, [("boss".to_string(), "turn-1".to_string(), 0.15)]);
    // 두 번째 턴 중 누적값이 줄면 CLI가 새로 시작한 것이라 0부터 센다.
    f.prompt("둘째 질문").await;
    status(0.05).await.unwrap();
    f.stop("답변").await.unwrap();
    assert_eq!(f.calls().completed[1].reported_cost_usd, Some(0.05));
    assert_eq!(f.calls().costs.len(), 1);
    f.manager.close("boss").await.unwrap();
}

#[tokio::test]
async fn claude_turn_cost_stays_empty_without_status_line() {
    let f = Fixture::new(Setup::backend("claude"));
    f.manager.open("boss").await.unwrap();
    f.prompt("질문").await;
    f.stop("답변").await.unwrap();
    assert_eq!(f.calls().completed[0].reported_cost_usd, None);
    f.manager.close("boss").await.unwrap();
}

// 완료 저장이 실패했을 때 턴을 running으로 남기면 이후 질문이 모두 거절된다.
#[tokio::test]
async fn claude_stop_save_failure_interrupts_turn_and_releases_session() {
    let f = Fixture::new(Setup::backend("claude"));
    lock(&f.runtime.complete_errors).push_back("터미널 최종 메시지가 없습니다.".into());
    f.manager.open("boss").await.unwrap();
    let started = f.prompt("첫 질문").await;
    assert!(f.stop("답변").await.is_err());
    let turn = started["turnId"].as_str().unwrap().to_string();
    assert_eq!(f.calls().interrupted, [("boss".to_string(), turn)]);
    let next = f.prompt("다음 질문").await;
    assert_eq!(next["accepted"], true);
    assert_ne!(next["turnId"], started["turnId"]);
    f.manager.close("boss").await.unwrap();
}

// 앱의 멈춤 버튼은 터미널에 Esc를 보낸 뒤 이 경로를 부른다. Claude는 중단 때 Stop 훅이 오지 않는다.
#[tokio::test]
async fn terminal_interrupt_closes_running_turn_and_releases_session() {
    let f = Fixture::new(Setup::backend("claude"));
    f.manager.open("boss").await.unwrap();
    assert!(f.manager.interrupt("left-man").await.is_err());
    assert_eq!(f.manager.interrupt("boss").await.unwrap(), json!({ "interrupted": false, "turnId": null }));
    let started = f.prompt("긴 작업").await;
    assert_eq!(f.manager.list()[0]["runningTurnId"], started["turnId"]);
    assert_eq!(f.manager.interrupt("boss").await.unwrap(), json!({ "interrupted": true, "turnId": started["turnId"] }));
    let turn = started["turnId"].as_str().unwrap().to_string();
    assert_eq!(f.calls().interrupted, [("boss".to_string(), turn)]);
    assert_eq!(f.manager.list()[0]["runningTurnId"], Value::Null);
    assert_eq!(f.calls().events.last().unwrap()["type"], "terminal.changed");
    // 중단 뒤 도착하는 Stop 훅은 풀린 세션을 다시 잠그지 않는다.
    let late = f.stop("늦은 답변").await.unwrap();
    assert_eq!(late["accepted"], false);
    assert!(f.calls().completed.is_empty());
    let next = f.prompt("다음 질문").await;
    assert_eq!(next["accepted"], true);
    assert_ne!(next["turnId"], started["turnId"]);
    f.manager.close("boss").await.unwrap();
}

// 터미널에서 직접 Esc를 눌러도 Stop 훅은 오지 않는다. 다음 질문이 남은 턴을 닫아야 기록이 이어진다.
#[tokio::test]
async fn new_prompt_without_stop_interrupts_previous_claude_turn() {
    let f = Fixture::new(Setup::backend("claude"));
    f.manager.open("boss").await.unwrap();
    let first = f.prompt("첫 질문").await;
    let second = f.prompt("Esc 뒤 두 번째 질문").await;
    assert_eq!(second["accepted"], true);
    assert_ne!(second["turnId"], first["turnId"]);
    let turn = first["turnId"].as_str().unwrap().to_string();
    assert_eq!(f.calls().interrupted, [("boss".to_string(), turn)]);
    assert_eq!(f.manager.list()[0]["runningTurnId"], second["turnId"]);
    assert_eq!(f.calls().begun_prompts(), ["첫 질문", "Esc 뒤 두 번째 질문"]);
    f.manager.close("boss").await.unwrap();
}

fn step(step_type: u64, status: f64, payload: Vec<u8>) -> StepRow {
    StepRow::new(
        SqliteValue::Number(1.0),
        SqliteValue::Number(step_type as f64),
        SqliteValue::Number(status),
        SqliteValue::Null,
        SqliteValue::Blob(payload),
    )
}

fn user_payload(text: &str) -> Vec<u8> {
    message(&[number_field(1, 14), bytes_field(19, &message(&[bytes_field(2, text.as_bytes())]))])
}

fn assistant_payload(text: &str) -> Vec<u8> {
    message(&[number_field(1, 15), bytes_field(20, &message(&[bytes_field(1, text.as_bytes())]))])
}

#[test]
fn antigravity_steps_14_and_15_decode_to_user_and_final_reply() {
    assert_eq!(parse_antigravity_terminal_step(&step(14, 3.0, user_payload("질문"))), Some(TerminalStep::User("질문".into())));
    match parse_antigravity_terminal_step(&step(15, 3.0, assistant_payload("답변"))) {
        Some(TerminalStep::Assistant { text, .. }) => assert_eq!(text, "답변"),
        other => panic!("최종 응답으로 해독되지 않았습니다: {other:?}"),
    }
}

const AGY_SESSION: &str = "01a0c0de-3333-7000-8000-000000000003";

/// 열기 전에 대화 DB와 steps 표를 만든다. Node 시험도 DB를 먼저 만든 뒤 터미널을 연다.
fn antigravity_setup(home: &TempDirectory) -> Setup {
    let conversations = home.join(&[".gemini", "antigravity-cli", "conversations"]);
    fs::create_dir_all(&conversations).unwrap();
    let database = rusqlite::Connection::open(join_path(&[&conversations, &format!("{AGY_SESSION}.db")])).unwrap();
    database
        .execute_batch("CREATE TABLE steps (idx INTEGER PRIMARY KEY, step_type INTEGER, status INTEGER, metadata BLOB, step_payload BLOB)")
        .unwrap();
    Setup { external_session_id: Some(AGY_SESSION.into()), ..Setup::backend("antigravity") }
}

fn steps_database(f: &Fixture) -> rusqlite::Connection {
    let path = f.home.join(&[".gemini", "antigravity-cli", "conversations", &format!("{AGY_SESSION}.db")]);
    rusqlite::Connection::open(path).unwrap()
}

fn insert_step(database: &rusqlite::Connection, index: i64, step_type: i64, status: i64, metadata: Option<Vec<u8>>, payload: Vec<u8>) {
    database
        .execute("INSERT INTO steps VALUES (?, ?, ?, ?, ?)", rusqlite::params![index, step_type, status, metadata, payload])
        .unwrap();
}

/// antigravityWatcherHarness. 배경 감시를 멈추고 sweep을 직접 부른다.
struct AgyHarness {
    f: Fixture,
    database: rusqlite::Connection,
}

impl AgyHarness {
    async fn new() -> Self {
        let f = Fixture::build(antigravity_setup, |_| {});
        let database = steps_database(&f);
        f.manager.open("boss").await.unwrap();
        f.stop_watcher().await;
        Self { f, database }
    }

    fn insert(&self, index: i64, kind: &str, text: &str, status: i64, metadata: Option<Vec<u8>>) {
        let step_type: u64 = match kind {
            "user" => 14,
            "assistant" => 15,
            "tool" => 132,
            "notification" => 101,
            other => panic!("모르는 단계 종류입니다: {other}"),
        };
        let body = match kind {
            "user" => bytes_field(19, &message(&[bytes_field(2, text.as_bytes())])),
            "assistant" => bytes_field(20, &message(&[bytes_field(1, text.as_bytes())])),
            _ => Vec::new(),
        };
        insert_step(&self.database, index, step_type as i64, status, metadata, message(&[number_field(1, step_type), body]));
    }

    fn insert_row(&self, row: &Value) {
        let text = row["text"].as_str().unwrap_or("");
        let status = row["status"].as_i64().unwrap_or(3);
        self.insert(row["idx"].as_i64().unwrap(), row["kind"].as_str().unwrap(), text, status, None);
    }

    async fn sweep(&self) -> Result<(), AgentError> {
        antigravity::sweep_locked(self.f.shared(), &self.f.session()).await
    }

    async fn pending_is_empty(&self) -> bool {
        let session = self.f.session();
        let context = session.turn.lock().await;
        context.antigravity.as_ref().unwrap().pending.is_empty()
    }

    async fn close(self) {
        self.f.manager.close("boss").await.unwrap();
    }
}

fn completed_turn_ids(calls: &Calls) -> Vec<&str> {
    calls.completed.iter().map(|turn| turn.turn_id.as_str()).collect()
}

fn completed_responses(calls: &Calls) -> Vec<&str> {
    calls.completed.iter().map(|turn| turn.response.as_str()).collect()
}

fn kinds(activities: &[Value]) -> Vec<&str> {
    activities.iter().filter_map(|activity| activity["kind"].as_str()).collect()
}

#[tokio::test]
async fn antigravity_poll_completes_turn_when_file_events_never_settle() {
    let f = Fixture::build(antigravity_setup, |options| {
        options.antigravity_poll_interval = Duration::from_millis(10);
        options.antigravity_debounce = Duration::from_millis(50);
    });
    let database = steps_database(&f);
    f.manager.open("boss").await.unwrap();
    // 실제 WAL/SHM처럼 debounce보다 잦은 파일 알림이 계속 와도 주기 확인은 굶지 않아야 한다.
    let storm_path = f.home.join(&[".gemini", "antigravity-cli", "conversations", "storm.txt"]);
    let storm = tokio::spawn(async move {
        loop {
            append(&storm_path, "x");
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    });
    insert_step(&database, 1, 14, 3, None, user_payload("질문"));
    wait_until(|| f.calls().begun.len() == 1).await;
    assert_eq!(f.running().as_deref(), Some("turn-1"));
    submit_structured_response_source(&f.structured_path(), json!({ "kind": "file", "title": "근거", "locator": "agy.mjs" })).unwrap();
    let reasoning = message(&[number_field(1, 15), bytes_field(20, &message(&[bytes_field(3, "공개 추론 요약".as_bytes())]))]);
    insert_step(&database, 2, 15, 3, None, reasoning);
    insert_step(&database, 3, 132, 3, None, message(&[number_field(1, 132)]));
    let answer = assistant_payload("답변");
    insert_step(&database, 4, 15, 1, None, answer.clone());
    // 미완료 행을 한 번 읽어 커서가 지나간 뒤 같은 idx가 완료되는 실제 기록 순서를 재현한다.
    let session = f.session();
    wait_until(|| session.turn.try_lock().ok().and_then(|context| context.antigravity.as_ref().map(|cursor| cursor.last_index)) == Some(4))
        .await;
    assert!(f.calls().completed.is_empty());
    database.execute("UPDATE steps SET status = 3, step_payload = ? WHERE idx = 4", [answer]).unwrap();
    wait_until(|| f.calls().completed.len() == 1).await;
    {
        let calls = f.calls();
        assert_eq!(calls.completed[0].response, "답변");
        assert_eq!(kinds(&calls.completed[0].activities), ["thinking", "tool"]);
        assert_eq!(calls.completed[0].structured.as_ref().unwrap().sources[0]["locator"], "agy.mjs");
    }
    // 다음 사용자 경계까지 같은 턴의 활동·근거를 보존한다.
    assert_eq!(session.lock_state().activities.finish("답변").len(), 2);
    assert_eq!(f.running(), None);
    storm.abort();
    f.manager.close("boss").await.unwrap();
}

#[tokio::test]
async fn antigravity_followup_replies_refresh_same_turn_without_leaking_into_next_question() {
    let h = AgyHarness::new().await;
    let fixture: Value = serde_json::from_str(include_str!("../../tests/fixtures/antigravity-background-replies.json")).unwrap();
    let snapshots = fixture["snapshots"].as_array().unwrap();
    for snapshot in snapshots {
        for row in snapshot.as_array().unwrap() {
            h.insert_row(row);
        }
        h.sweep().await.unwrap();
    }
    {
        let calls = h.f.calls();
        assert_eq!(calls.begun.len(), 2);
        assert_eq!(completed_turn_ids(&calls), ["turn-1", "turn-2", "turn-2", "turn-2", "turn-2"]);
        let refreshed: Vec<bool> = calls.completed.iter().map(|turn| turn.refresh_completed).collect();
        assert_eq!(refreshed, [false, false, true, true, true]);
        let last = calls.completed.last().unwrap();
        assert_eq!(last.response, snapshots[4][1]["text"].as_str().unwrap());
        let messages: Vec<&str> =
            last.activities.iter().filter(|activity| activity["kind"] == "message").filter_map(|activity| activity["text"].as_str()).collect();
        let expected: Vec<&str> = (1..=3).map(|index| snapshots[index][2]["text"].as_str().unwrap()).collect();
        assert_eq!(messages, expected);
    }
    assert!(h.pending_is_empty().await);
    h.sweep().await.unwrap();
    assert_eq!(h.f.calls().completed.len(), 5, "변경 없는 재조회는 중복 저장하지 않는다");
    h.insert(4344, "user", "새 질문", 3, None);
    h.insert(4345, "assistant", "새 답변", 3, None);
    h.sweep().await.unwrap();
    {
        let calls = h.f.calls();
        let last = calls.completed.last().unwrap();
        assert_eq!(last.turn_id, "turn-3");
        assert!(last.activities.is_empty());
    }
    h.close().await;
}

#[tokio::test]
async fn antigravity_late_previous_step_and_orphan_reply_do_not_attach_to_new_question() {
    let h = AgyHarness::new().await;
    h.insert(1, "assistant", "모드 진입 전 응답", 3, None);
    h.insert(2, "user", "응답 전 중단할 질문", 3, None);
    h.insert(3, "assistant", "늦은 이전 답변", 1, None);
    h.sweep().await.unwrap();
    h.insert(4, "user", "지금 질문", 3, None);
    h.insert(5, "assistant", "지금 답변", 3, None);
    h.sweep().await.unwrap();
    h.database.execute("UPDATE steps SET status = 3 WHERE idx = 3", []).unwrap();
    h.sweep().await.unwrap();
    {
        let calls = h.f.calls();
        let interrupted: Vec<&str> = calls.interrupted.iter().map(|(_, turn)| turn.as_str()).collect();
        assert_eq!(interrupted, ["turn-1"]);
        assert_eq!(completed_responses(&calls), ["지금 답변"]);
        assert!(calls.completed[0].activities.is_empty());
    }
    assert!(h.pending_is_empty().await);
    h.close().await;
}

#[tokio::test]
async fn antigravity_followup_keeps_all_step_usage_and_sources_without_double_counting_retry() {
    let h = AgyHarness::new().await;
    let metadata = |input: u64, output: u64| {
        Some(message(&[
            bytes_field(12, b"execution"),
            bytes_field(1, &message(&[number_field(1, 1_788_618_000)])),
            bytes_field(9, &message(&[number_field(2, input), number_field(3, output)])),
        ]))
    };
    h.insert(1, "user", "질문", 3, None);
    h.sweep().await.unwrap();
    let result_path = h.f.structured_path();
    submit_structured_response_source(&result_path, json!({ "kind": "file", "title": "첫 근거", "locator": "first.mjs" })).unwrap();
    h.insert(2, "assistant", "", 3, metadata(100, 10));
    h.insert(3, "assistant", "작업 중", 3, metadata(50, 5));
    h.sweep().await.unwrap();
    assert_eq!(number(&h.f.calls().completed[0].usage.as_ref().unwrap()["inputTokens"]), 150.0);
    submit_structured_response_source(&result_path, json!({ "kind": "file", "title": "후속 근거", "locator": "last.mjs" })).unwrap();
    h.insert(4, "assistant", "최종 답변", 3, metadata(25, 3));
    lock(&h.f.runtime.complete_errors).push_back("일시적 저장 실패".into());
    let failure = h.sweep().await.expect_err("첫 저장은 실패해야 한다");
    assert!(failure.to_string().contains("일시적 저장 실패"));
    h.sweep().await.unwrap();
    h.sweep().await.unwrap();
    {
        let calls = h.f.calls();
        assert_eq!(calls.completed.len(), 2);
        let usage = calls.completed[1].usage.as_ref().unwrap();
        assert_eq!(number(&usage["inputTokens"]), 175.0);
        assert_eq!(number(&usage["outputTokens"]), 18.0);
        let locators: Vec<&str> =
            calls.completed[1].structured.as_ref().unwrap().sources.iter().filter_map(|source| source["locator"].as_str()).collect();
        assert_eq!(locators, ["first.mjs", "last.mjs"]);
        assert_eq!(calls.completed[1].response, "최종 답변");
    }
    h.close().await;
}

#[tokio::test]
async fn antigravity_late_reply_after_explicit_interrupt_does_not_revive_turn() {
    let h = AgyHarness::new().await;
    h.insert(1, "user", "중단할 질문", 3, None);
    h.sweep().await.unwrap();
    h.f.manager.interrupt("boss").await.unwrap();
    h.insert(2, "assistant", "중단 뒤 늦은 응답", 3, None);
    h.sweep().await.unwrap();
    assert!(h.f.calls().completed.is_empty());
    h.insert(3, "user", "다음 질문", 3, None);
    h.insert(4, "assistant", "다음 답변", 3, None);
    h.sweep().await.unwrap();
    {
        let calls = h.f.calls();
        assert_eq!(completed_responses(&calls), ["다음 답변"]);
        assert!(calls.completed[0].activities.is_empty());
    }
    h.close().await;
}

const CODEX_THREAD: &str = "01a0c0de-1111-7000-8000-000000000001";
const CODEX_TURN: &str = "01a0c0de-2222-7000-8000-000000000002";

fn rollout_path(home: &TempDirectory) -> String {
    home.join(&[".codex", "sessions", "2026", "09", "03", &format!("rollout-2026-09-03T00-00-00-{CODEX_THREAD}.jsonl")])
}

/// codexFixture. 열기 전에 이미 끝난 지난 턴이 있는 rollout. 감시기가 되풀이하면 안 된다.
fn codex_setup(home: &TempDirectory) -> Setup {
    let workdir = home.join(&["workdir"]);
    fs::create_dir_all(&workdir).unwrap();
    let rollout = rollout_path(home);
    fs::create_dir_all(Path::new(&rollout).parent().unwrap()).unwrap();
    let old = [
        json!({ "type": "session_meta", "payload": { "id": CODEX_THREAD, "source": "cli", "cwd": workdir } }),
        json!({ "type": "event_msg", "payload": { "type": "task_started", "turn_id": "old-turn", "started_at": 1 } }),
        json!({ "type": "event_msg", "payload": { "type": "item_completed", "turn_id": "old-turn",
            "item": { "type": "UserMessage", "content": [{ "type": "text", "text": "지난 질문" }] } } }),
        json!({ "type": "event_msg", "payload": { "type": "task_complete", "turn_id": "old-turn", "last_agent_message": "지난 답변",
            "started_at": 1, "completed_at": 2 } }),
    ];
    fs::write(&rollout, lines(&old)).unwrap();
    Setup { external_session_id: Some(CODEX_THREAD.into()), workdir, ..Setup::backend("codex") }
}

fn task_started() -> Value {
    json!({ "type": "event_msg", "payload": { "type": "task_started", "turn_id": CODEX_TURN, "started_at": 1_788_347_510 } })
}

fn codex_user(text: &str) -> Value {
    json!({ "type": "event_msg", "payload": { "type": "item_completed", "turn_id": CODEX_TURN,
        "item": { "type": "UserMessage", "content": [{ "type": "text", "text": text }] } } })
}

fn task_complete(message: &str, completed_at: u64) -> Value {
    json!({ "type": "event_msg", "payload": { "type": "task_complete", "turn_id": CODEX_TURN, "last_agent_message": message,
        "started_at": 1_788_347_510, "completed_at": completed_at } })
}

fn commentary(text: &str) -> Value {
    json!({ "type": "response_item", "payload": { "type": "message", "role": "assistant", "phase": "commentary",
        "content": [{ "type": "output_text", "text": text }] } })
}

/// 배경 주기 확인을 멈추고 sweep을 직접 부른다(Node state.watcher.sweep).
struct CodexHarness {
    f: Fixture,
    rollout: String,
    workdir: String,
}

impl CodexHarness {
    async fn new() -> Self {
        let f = Fixture::build(codex_setup, |_| {});
        let rollout = rollout_path(&f.home);
        let workdir = f.home.join(&["workdir"]);
        f.manager.open("boss").await.unwrap();
        f.stop_watcher().await;
        Self { f, rollout, workdir }
    }

    fn append(&self, records: &[Value]) {
        append(&self.rollout, &lines(records));
    }

    async fn sweep(&self) {
        codex::sweep_locked(self.f.shared(), &self.f.session()).await.unwrap();
    }

    /// codexNotify.
    async fn notify(&self, message: &str) -> Value {
        let payload = json!({
            "type": "agent-turn-complete",
            "thread-id": CODEX_THREAD,
            "turn-id": CODEX_TURN,
            "cwd": self.workdir,
            "last-assistant-message": message,
        });
        self.f.manager.handle_event("boss", json!({ "source": "codex", "payload": payload })).await.unwrap()
    }

    async fn close(self) {
        self.f.manager.close("boss").await.unwrap();
    }
}

#[tokio::test]
async fn codex_rollout_start_and_prompt_create_running_turn_that_notify_completes() {
    let h = CodexHarness::new().await;
    // 열기 전 기록은 되풀이하지 않는다.
    h.sweep().await;
    assert!(h.f.calls().begun.is_empty());
    h.append(&[task_started(), codex_user("터미널 질문")]);
    h.f.calls().events.clear();
    h.sweep().await;
    {
        let calls = h.f.calls();
        assert_eq!(calls.begun_prompts(), ["터미널 질문"]);
        assert!(calls.has_event("terminal.changed"));
    }
    assert_eq!(h.f.running().as_deref(), Some("turn-1"));
    // 같은 줄을 다시 읽어도 두 번 만들지 않는다.
    h.sweep().await;
    assert_eq!(h.f.calls().begun.len(), 1);
    h.append(&[
        json!({ "type": "turn_context", "payload": { "turn_id": CODEX_TURN, "cwd": h.workdir } }),
        commentary("작업 진행"),
        json!({ "type": "response_item", "payload": { "type": "reasoning", "summary": [{ "type": "summary_text", "text": "공개 요약" }] } }),
        json!({ "type": "event_msg", "payload": { "type": "item_completed", "turn_id": CODEX_TURN,
            "item": { "type": "AgentMessage", "phase": "final_answer", "content": [{ "type": "Text", "text": "터미널 답변" }] } } }),
        task_complete("터미널 답변", 1_788_347_513),
    ]);
    submit_structured_response_source(&h.f.structured_path(), json!({ "kind": "file", "title": "근거", "locator": "codex.mjs" })).unwrap();
    let completed = h.notify("터미널 답변").await;
    assert_eq!(completed["accepted"], true);
    assert_eq!(completed["turnId"], "turn-1");
    {
        let calls = h.f.calls();
        assert_eq!(calls.begun.len(), 1);
        let done: Vec<(&str, &str)> = calls.completed.iter().map(|turn| (turn.turn_id.as_str(), turn.response.as_str())).collect();
        assert_eq!(done, [("turn-1", "터미널 답변")]);
        assert_eq!(kinds(&calls.completed[0].activities), ["message", "thinking"]);
        assert_eq!(calls.completed[0].structured.as_ref().unwrap().sources[0]["locator"], "codex.mjs");
    }
    assert_eq!(h.f.running(), None);
    assert_eq!(h.notify("터미널 답변").await["reason"], "duplicate");
    assert_eq!(h.f.calls().completed.len(), 1);
    h.close().await;
}

#[tokio::test]
async fn codex_terminal_broadcasts_only_current_operation_before_notify_excluding_foreign_turns() {
    let h = CodexHarness::new().await;
    h.sweep().await;
    h.append(&[
        task_started(),
        codex_user("질문"),
        commentary("원인을 확인 중입니다"),
        json!({ "type": "response_item", "payload": { "type": "function_call", "name": "exec_command", "call_id": "live",
            "arguments": json!({ "cmd": "swift test" }).to_string() } }),
    ]);
    h.sweep().await;
    let statuses = |calls: &Calls| -> Vec<(String, String)> {
        calls
            .progress
            .iter()
            .map(|activity| (activity["kind"].as_str().unwrap_or("").into(), activity["status"].as_str().unwrap_or("").into()))
            .collect()
    };
    {
        let calls = h.f.calls();
        assert!(calls.completed.is_empty());
        assert_eq!(statuses(&calls), [("command".to_string(), "running".to_string())]);
    }
    let count = h.f.calls().progress.len();
    h.sweep().await;
    assert_eq!(h.f.calls().progress.len(), count);
    h.append(&[json!({ "type": "response_item", "payload": { "type": "function_call_output", "call_id": "live",
        "output": json!({ "exit_code": 0 }).to_string() } })]);
    h.sweep().await;
    {
        let calls = h.f.calls();
        let last = calls.progress.last().unwrap();
        assert_eq!(last["status"], "completed");
        assert_eq!(last["text"], "swift test");
    }
    h.append(&[json!({ "type": "turn_context", "payload": { "turn_id": "another-turn" } }), commentary("다른 턴")]);
    h.sweep().await;
    assert!(!h.f.calls().progress.iter().any(|activity| activity["text"] == "다른 턴"));
    h.close().await;
}

#[tokio::test]
async fn codex_notify_before_watcher_records_turn_and_late_sweep_does_not_recreate_it() {
    let h = CodexHarness::new().await;
    h.sweep().await;
    h.append(&[
        task_started(),
        json!({ "type": "turn_context", "payload": { "turn_id": CODEX_TURN, "cwd": h.workdir } }),
        codex_user("빠른 질문"),
        task_complete("빠른 답변", 1_788_347_511),
    ]);
    let completed = h.notify("빠른 답변").await;
    assert_eq!(completed["accepted"], true);
    {
        let calls = h.f.calls();
        assert_eq!(calls.begun.len(), 1);
        assert_eq!(calls.completed.len(), 1);
    }
    assert_eq!(h.f.running(), None);
    h.sweep().await;
    assert_eq!(h.f.calls().begun.len(), 1);
    h.close().await;
}

#[tokio::test]
async fn codex_turn_aborted_interrupts_running_turn() {
    let h = CodexHarness::new().await;
    h.sweep().await;
    h.append(&[task_started(), codex_user("중단할 질문")]);
    h.sweep().await;
    assert_eq!(h.f.running().as_deref(), Some("turn-1"));
    h.f.calls().events.clear();
    h.append(&[json!({ "type": "event_msg", "payload": { "type": "turn_aborted", "turn_id": CODEX_TURN,
        "started_at": 1_788_347_510, "completed_at": 1_788_347_511 } })]);
    h.sweep().await;
    {
        let calls = h.f.calls();
        assert_eq!(calls.interrupted, [("boss".to_string(), "turn-1".to_string())]);
        assert!(calls.has_event("terminal.changed"));
    }
    assert_eq!(h.f.running(), None);
    assert_eq!(h.f.session().lock_state().codex_turn_id, None);
    h.close().await;
}

/// Node는 sweep을 직접 불렀다. 배경 주기 확인이 실제로 같은 일을 하는지도 본다.
#[tokio::test]
async fn codex_background_poll_creates_running_turn_without_manual_sweep() {
    let f = Fixture::build(codex_setup, |options| options.codex_poll_interval = Duration::from_millis(20));
    let rollout = rollout_path(&f.home);
    f.manager.open("boss").await.unwrap();
    // 감시기는 첫 확인 때 파일 끝을 잡는다. 그 전에 붙인 줄은 지난 기록으로 본다(Node와 같다).
    codex::sweep_locked(f.shared(), &f.session()).await.unwrap();
    append(&rollout, &lines(&[task_started(), codex_user("배경 질문")]));
    wait_until(|| f.calls().begun.len() == 1).await;
    assert_eq!(f.calls().begun_prompts(), ["배경 질문"]);
    assert_eq!(f.running().as_deref(), Some("turn-1"));
    f.manager.close("boss").await.unwrap();
}

fn start_request(prompt: &str) -> StartRequest {
    StartRequest { character_id: "boss".into(), prompt: Some(json!(prompt)), ..StartRequest::default() }
}

fn is_busy<T>(result: &Result<T, AgentError>) -> bool {
    matches!(result, Err(AgentError::Busy(_)))
}

/// dispatchFixture의 도우미. 소유 토큰은 본문에만 넣고 출력하지 않는다.
impl Fixture {
    fn dispatch(&self, request: StartRequest) -> JoinHandle<Result<Value, AgentError>> {
        tokio::spawn(Arc::clone(self.shared()).dispatch(request))
    }

    fn control(&self, action: &str, extra: Value) -> Result<Value, AgentError> {
        let session = self.session();
        let dispatch_id = session.lock_state().dispatch.as_ref().map(|pending| pending.id.clone());
        let body = json!({
            "action": action,
            "dispatchId": dispatch_id,
            "terminalSessionId": session.terminal_session_id,
            "ownerToken": session.owner_token,
        });
        self.manager.dispatch_control("boss", &with(body, extra))
    }

    fn claim(&self) -> String {
        match self.control("claim", json!({})) {
            Ok(claimed) => claimed["prompt"].as_str().expect("전달 본문").to_string(),
            Err(error) => panic!("claim이 거절되었습니다: {error}"),
        }
    }

    fn pending(&self) -> bool {
        self.session().lock_state().dispatch.is_some()
    }

    fn wired(&self) -> bool {
        self.session().lock_state().dispatch.as_ref().is_some_and(|pending| pending.wire.is_some())
    }

    fn expired(&self) -> bool {
        self.session().lock_state().dispatch.as_ref().is_some_and(|pending| pending.expired)
    }

    fn dispatched(&self) -> bool {
        self.calls().has_event("terminal.dispatch")
    }

    /// session_id 없는 UserPromptSubmit(Node submit).
    async fn submit(&self, prompt: &str) -> Value {
        self.claude(json!({ "hook_event_name": "UserPromptSubmit", "prompt": prompt })).await.unwrap()
    }

    async fn reply(&self, text: &str) {
        self.claude(json!({ "hook_event_name": "Stop", "last_assistant_message": text })).await.unwrap();
    }

    fn work_status(&self) -> Vec<Value> {
        self.calls().events.iter().filter(|event| event["type"] == "terminal.work-status").cloned().collect()
    }
}

async fn dispatch_fixture(backend: &'static str, timeout: Duration) -> Fixture {
    let f = Fixture::build(|_| Setup::backend(backend), |options| options.dispatch_timeout = timeout);
    f.manager.open("boss").await.unwrap();
    f
}

const SECOND: Duration = Duration::from_secs(1);

/// Node는 AgentRuntime.start를 거쳤다. 여기서는 관리자 전달만 보고, 실행기 경유는 DB 통합 시험이 맡는다.
#[tokio::test]
async fn idle_terminal_dispatch_returns_turn_only_after_same_cli_hook_accepts() {
    let f = dispatch_fixture("claude", SECOND).await;
    let request = StartRequest {
        sender_character_id: Some(json!("right-man")),
        request_source: Some("remote".into()),
        ..start_request("답변해줘")
    };
    let result = f.dispatch(request);
    wait_until(|| f.dispatched()).await;
    assert!(f.calls().begun.is_empty());
    let prompt = f.claim();
    tokio::time::sleep(Duration::from_millis(5)).await;
    assert!(!result.is_finished(), "PTY에 쓴 것만으로는 접수가 아니다");
    let started = f.submit(&prompt).await;
    let settled = result.await.unwrap().unwrap();
    assert_eq!(settled, json!({ "turnId": started["turnId"], "conversationId": "conversation", "status": "running" }));
    {
        let calls = f.calls();
        assert_eq!(calls.begun[0].prompt, "답변해줘");
        assert_eq!(calls.begun[0].sender_character_id, Some(json!("right-man")));
        assert_eq!(calls.begun[0].request_source.as_deref(), Some("remote"));
    }
    assert_eq!(f.manager.size(), 1);
    f.reply("회신").await;
    assert_eq!(json!(f.calls().completed[0].turn_id), started["turnId"]);
    f.manager.close("boss").await.unwrap();
}

/// replyRecipientID는 Rust 전달 기록에 남기지 않는다(의도한 차이). 전달 ID만 본다.
#[tokio::test]
async fn terminal_without_saved_tags_adds_no_routing_guidance() {
    let f = dispatch_fixture("claude", SECOND).await;
    let request = StartRequest {
        sender_character_id: Some(json!("left-woman")),
        reply_recipient_id: Some("right-woman".into()),
        delivery_id: Some("delivery-test".into()),
        ..start_request("자연스럽게 답해줘")
    };
    let result = f.dispatch(request);
    wait_until(|| f.dispatched()).await;
    let prompt = f.claim();
    assert!(!prompt.contains("OFFICESTRA 전달 안내") && !prompt.contains("자동 전달"));
    let started = f.submit(&prompt).await;
    result.await.unwrap().unwrap();
    {
        let calls = f.calls();
        assert_eq!(calls.begun[0].prompt, "자연스럽게 답해줘");
        assert_eq!(calls.begun[0].delivery_id.as_deref(), Some("delivery-test"));
    }
    f.reply("네, 좋아요.").await;
    assert_eq!(json!(f.calls().completed[0].turn_id), started["turnId"]);
    f.manager.close("boss").await.unwrap();
}

#[tokio::test]
async fn terminal_routing_guidance_uses_only_receiving_employee_saved_tags() {
    let f = dispatch_fixture("claude", SECOND).await;
    *lock(&f.runtime.tags) = vec!["left-man".into(), "right-woman".into()];
    f.runtime.hold_tags.store(true, Ordering::SeqCst);
    let result = f.dispatch(StartRequest { reply_recipient_id: Some("left-woman".into()), ..start_request("검토해줘") });
    wait_until(|| f.pending()).await;
    assert!(is_busy(&f.control("claim", json!({}))), "태그와 바쁨 확인 전에는 가져갈 수 없다");
    f.runtime.release_tags.notify_one();
    wait_until(|| f.dispatched()).await;
    let prompt = f.claim();
    assert!(prompt.contains(r#"선택된 자동 전달 태그=["left-man","right-woman"]"#));
    assert!(!prompt.contains("left-woman"));
    assert_eq!(f.calls().tag_queries, ["boss"]);
    f.submit(&prompt).await;
    result.await.unwrap().unwrap();
    assert_eq!(f.calls().begun[0].prompt, "검토해줘");
    f.manager.close("boss").await.unwrap();
}

#[tokio::test]
async fn claude_long_paste_acknowledgement_preserves_dispatch_identity_and_releases_lock() {
    let f = dispatch_fixture("claude", SECOND).await;
    let request = StartRequest {
        sender_character_id: Some(json!("left-woman")),
        reply_recipient_id: Some("right-woman".into()),
        delivery_id: Some("paste-delivery".into()),
        ..start_request("긴 메시지\n검토 요청")
    };
    let result = f.dispatch(request);
    wait_until(|| f.wired()).await;
    let prompt = f.claim();
    f.submit(&format!("<pasted_content id=\"33ad\">\n{prompt}\n</pasted_content id=\"33ad\">")).await;
    assert!(!f.pending());
    result.await.unwrap().unwrap();
    {
        let calls = f.calls();
        assert_eq!(calls.begun[0].prompt, "긴 메시지\n검토 요청");
        assert_eq!(calls.begun[0].sender_character_id, Some(json!("left-woman")));
        assert_eq!(calls.begun[0].delivery_id.as_deref(), Some("paste-delivery"));
    }
    f.reply("확인").await;
    let second = f.dispatch(start_request("후속 요청"));
    wait_until(|| f.wired()).await;
    let prompt = f.claim();
    f.submit(&prompt).await;
    second.await.unwrap().unwrap();
    f.manager.close("boss").await.unwrap();
}

/// Node는 execution.backend만 바꿨다. Rust는 세션마다 backend가 고정이라 변형마다 그 backend로 연다.
#[tokio::test]
async fn paste_acknowledgement_rejects_changed_extra_mismatched_unclaimed_and_other_backends() {
    for variant in ["changed", "extra", "id", "unclaimed", "codex", "antigravity"] {
        let backend = if matches!(variant, "codex" | "antigravity") { variant } else { "claude" };
        let f = dispatch_fixture(backend, SECOND).await;
        let request = StartRequest {
            sender_character_id: Some(json!("left-woman")),
            request_source: Some("remote".into()),
            ..start_request("원문")
        };
        let result = f.dispatch(request);
        wait_until(|| f.wired()).await;
        let prompt = if variant == "unclaimed" {
            f.session().lock_state().dispatch.as_ref().and_then(|pending| pending.wire.clone()).unwrap()
        } else {
            f.claim()
        };
        let changed = if variant == "changed" { " 변경" } else { "" };
        let closing_id = if variant == "id" { "other" } else { "33ad" };
        let mut wrapped = format!("<pasted_content id=\"33ad\">\n{prompt}{changed}\n</pasted_content id=\"{closing_id}\">");
        if variant == "extra" {
            wrapped.push_str("\n다른 입력");
        }
        f.shared().begin_dispatched_turn(&f.session(), wrapped.clone(), None).await.unwrap();
        assert!(f.pending(), "{variant}: 전달을 확정하면 안 된다");
        {
            let calls = f.calls();
            assert_eq!(calls.begun[0].prompt, wrapped, "{variant}");
            assert_eq!(calls.begun[0].sender_character_id, None, "{variant}");
            assert_eq!(calls.begun[0].request_source, None, "{variant}");
        }
        f.manager.close("boss").await.unwrap();
        assert!(is_busy(&result.await.unwrap()), "{variant}");
    }
}

#[tokio::test]
async fn duplicate_db_running_and_compacting_dispatch_are_rejected_immediately() {
    let f = dispatch_fixture("claude", SECOND).await;
    f.runtime.dispatch_busy.store(true, Ordering::SeqCst);
    assert!(is_busy(&f.dispatch(start_request("a")).await.unwrap()));
    assert!(!f.pending());
    f.runtime.dispatch_busy.store(false, Ordering::SeqCst);
    lock(&f.runtime.compacting).insert("boss".into());
    assert!(is_busy(&f.dispatch(start_request("a")).await.unwrap()));
    lock(&f.runtime.compacting).clear();
    let result = f.dispatch(start_request("a"));
    wait_until(|| f.pending()).await;
    assert!(is_busy(&f.dispatch(start_request("b")).await.unwrap()));
    f.manager.close("boss").await.unwrap();
    assert!(is_busy(&result.await.unwrap()));
    assert!(f.calls().begun.is_empty());
}

#[tokio::test]
async fn ownership_epoch_duplicate_claim_and_mixed_user_input_do_not_confirm_delivery() {
    let f = dispatch_fixture("claude", SECOND).await;
    let result = f.dispatch(start_request("API 요청"));
    wait_until(|| f.dispatched()).await;
    assert!(is_busy(&f.control("claim", json!({ "ownerToken": "wrong" }))));
    assert!(is_busy(&f.control("claim", json!({ "terminalSessionId": "old-session" }))));
    let prompt = f.claim();
    assert!(is_busy(&f.control("claim", json!({}))));
    f.submit(&format!("{prompt} altered")).await;
    assert!(f.pending());
    {
        let calls = f.calls();
        assert_eq!(calls.begun[0].prompt, format!("{prompt} altered"));
        assert_eq!(calls.begun[0].sender_character_id, None);
    }
    f.manager.close("boss").await.unwrap();
    assert!(is_busy(&result.await.unwrap()));
}

#[tokio::test]
async fn app_rejecting_input_conflict_ends_without_turn() {
    let f = dispatch_fixture("claude", SECOND).await;
    let result = f.dispatch(start_request("a"));
    wait_until(|| f.dispatched()).await;
    f.claim();
    f.control("reject", json!({})).unwrap();
    assert!(is_busy(&result.await.unwrap()));
    assert!(!f.pending());
    assert!(f.calls().begun.is_empty());
    f.manager.close("boss").await.unwrap();
}

/// Node는 35ms 기한이었다. 병렬 실행에서 claim이 기한보다 늦지 않도록 300ms로 둔다. 기대값은 같다.
/// claim 뒤 기한이 지나면 Node는 AgentBusyError, Rust는 DeliveryUncertain이다(같은 409, 의도한 차이).
#[tokio::test]
async fn invalid_control_body_is_busy_and_late_reject_releases_lock() {
    let f = dispatch_fixture("claude", Duration::from_millis(300)).await;
    for body in [Value::Null, json!([]), json!("claim")] {
        assert!(is_busy(&f.manager.dispatch_control("boss", &body)));
    }
    let result = f.dispatch(start_request("전송 안 됨"));
    wait_until(|| f.dispatched()).await;
    f.claim();
    assert!(matches!(result.await.unwrap(), Err(AgentError::DeliveryUncertain(_))));
    assert!(f.expired());
    assert!(is_busy(&f.control("claim", json!({}))));
    f.control("reject", json!({})).unwrap();
    assert!(!f.pending());
    assert!(f.calls().begun.is_empty());
    f.manager.close("boss").await.unwrap();
}

#[tokio::test]
async fn unclaimed_timeout_releases_but_claimed_timeout_blocks_retry_and_accepts_late_hook() {
    let f = dispatch_fixture("claude", Duration::from_millis(300)).await;
    assert!(is_busy(&f.dispatch(start_request("미수신")).await.unwrap()));
    assert!(!f.pending());
    let result = f.dispatch(start_request("늦은 접수"));
    wait_until(|| f.wired()).await;
    let prompt = f.claim();
    assert!(matches!(result.await.unwrap(), Err(AgentError::DeliveryUncertain(_))));
    assert!(f.expired());
    assert!(is_busy(&f.dispatch(start_request("재시도")).await.unwrap()));
    f.submit(&prompt).await;
    assert!(!f.pending());
    assert_eq!(f.calls().begun[0].prompt, "늦은 접수");
    f.manager.close("boss").await.unwrap();
}

#[tokio::test]
async fn close_and_interrupt_clear_uncertain_delivery_and_control_characters_are_rejected() {
    let f = dispatch_fixture("claude", SECOND).await;
    for prompt in ["", "a\u{1b}b", "a\rb", "a\0b"] {
        assert!(f.dispatch(start_request(prompt)).await.unwrap().is_err(), "{prompt:?}");
    }
    let other = StartRequest { conversation_id: Some(json!("other")), ..start_request("x") };
    assert!(is_busy(&f.dispatch(other).await.unwrap()));
    let result = f.dispatch(StartRequest { attachment_paths: Some(json!(["/tmp/example.png"])), ..start_request("x") });
    wait_until(|| f.wired()).await;
    let prompt = f.claim();
    assert!(prompt.ends_with("/tmp/example.png"));
    f.manager.interrupt("boss").await.unwrap();
    assert!(is_busy(&result.await.unwrap()));
    assert!(!f.pending());
    f.manager.close("boss").await.unwrap();
}

#[tokio::test]
async fn common_turn_start_keeps_real_prompt_and_turn_for_three_backends() {
    for backend in ["claude", "codex", "antigravity"] {
        let f = dispatch_fixture(backend, SECOND).await;
        let result = f.dispatch(start_request("같은 대화"));
        wait_until(|| f.wired()).await;
        let prompt = f.claim();
        let turn = f.shared().begin_dispatched_turn(&f.session(), prompt, None).await.unwrap();
        assert_eq!(result.await.unwrap().unwrap()["turnId"], turn, "{backend}");
        {
            let calls = f.calls();
            assert_eq!(calls.begun[0].execution.backend, backend);
            assert_eq!(calls.begun[0].prompt, "같은 대화");
        }
        f.manager.close("boss").await.unwrap();
    }
}

#[tokio::test]
async fn antigravity_sqlite_watcher_links_api_marked_turn_and_completes_once() {
    let h = AgyHarness::new().await;
    let result = h.f.dispatch(start_request("안티 회신"));
    wait_until(|| h.f.wired()).await;
    let prompt = h.f.claim();
    h.insert(1, "user", &prompt, 3, None);
    h.sweep().await.unwrap();
    assert_eq!(result.await.unwrap().unwrap()["turnId"], json!(h.f.running()));
    assert_eq!(h.f.calls().begun[0].prompt, "안티 회신");
    h.insert(2, "assistant", "확인했습니다", 3, None);
    h.sweep().await.unwrap();
    h.sweep().await.unwrap();
    {
        let calls = h.f.calls();
        assert_eq!(calls.begun.len(), 1);
        assert_eq!(calls.completed.len(), 1);
    }
    h.close().await;
}

#[tokio::test]
async fn codex_rollout_watcher_and_notify_accept_and_complete_api_request_as_same_turn() {
    let h = CodexHarness::new().await;
    h.sweep().await;
    let result = h.f.dispatch(start_request("코덱스 회신"));
    wait_until(|| h.f.wired()).await;
    let prompt = h.f.claim();
    h.append(&[
        json!({ "type": "turn_context", "payload": { "turn_id": CODEX_TURN, "cwd": h.workdir } }),
        task_started(),
        codex_user(&prompt),
    ]);
    h.sweep().await;
    assert_eq!(result.await.unwrap().unwrap()["turnId"], json!(h.f.running()));
    assert_eq!(h.f.calls().begun[0].prompt, "코덱스 회신");
    h.append(&[task_complete("회신 완료", 1_788_347_513)]);
    h.notify("회신 완료").await;
    {
        let calls = h.f.calls();
        assert_eq!(calls.begun.len(), 1);
        assert_eq!(calls.completed.len(), 1);
    }
    h.close().await;
}

// terminal-live-progress.test.mjs

#[test]
fn live_snapshot_keeps_running_then_failure_and_hides_command_credentials() {
    let mut collector = TerminalActivityCollector::default();
    let command = json!({ "cmd": "curl -H 'Authorization: Bearer secret123' https://example.test" }).to_string();
    collector
        .codex(&json!({ "type": "response_item", "payload": { "type": "function_call", "call_id": "c1", "name": "exec_command", "arguments": command } }))
        .unwrap();
    let snapshot = collector.snapshot("", false);
    assert_eq!(snapshot[0]["status"], "running");
    assert!(!snapshot[0]["text"].as_str().unwrap().contains("secret123"));
    collector
        .codex(&json!({ "type": "response_item", "payload": { "type": "function_call_output", "call_id": "c1",
            "output": json!({ "exit_code": 1 }).to_string() } }))
        .unwrap();
    let snapshot = collector.snapshot("", false);
    assert_eq!(snapshot.len(), 1);
    assert_eq!(snapshot[0]["status"], "failed");
    assert_eq!(collector.finish("")[0]["eventKey"], snapshot[0]["eventKey"]);
}

#[test]
fn long_final_answer_stays_bounded_live_and_is_excluded_from_final_import() {
    let mut collector = TerminalActivityCollector::default();
    let response = "긴 답변".repeat(2000);
    collector.add(&json!({ "kind": "message", "text": response, "eventKey": "final" }));
    assert_eq!(collector.snapshot("", false)[0]["text"].as_str().unwrap().encode_utf16().count(), 6000);
    assert!(collector.finish(&response).is_empty());
}

/// harness(). workdir /repo, 진행 중 turn-1, 외부 세션 session-1.
async fn progress_fixture() -> Fixture {
    let f = Fixture::new(Setup { external_session_id: Some("session-1".into()), workdir: "/repo".into(), ..Setup::backend("claude") });
    f.manager.open("boss").await.unwrap();
    {
        let session = f.session();
        let mut state = session.lock_state();
        state.running_turn_id = Some("turn-1".into());
        state.activities = TerminalActivityCollector::new(Some("/repo".into()));
    }
    f
}

/// 가짜 실행기에는 활동 저장 경로가 없다. Node의 "DB 쓰기 0건"은 구조상 항상 성립한다.
#[tokio::test]
async fn bubble_status_broadcasts_only_latest_operation() {
    let f = progress_fixture().await;
    let session = f.session();
    session.lock_state().activities.add(&json!({ "kind": "command", "text": "swift test", "eventKey": "test", "status": "running" }));
    progress::publish(f.shared(), &session);
    assert_eq!(
        f.work_status(),
        [json!({ "type": "terminal.work-status", "characterId": "boss", "turnId": "turn-1",
            "workActivity": { "kind": "command", "text": "swift test", "status": "running" } })]
    );
    progress::publish(f.shared(), &session);
    assert_eq!(f.work_status().len(), 1);
    session.lock_state().activities.add(&json!({ "kind": "command", "text": "swift test", "eventKey": "test", "status": "completed" }));
    progress::publish(f.shared(), &session);
    assert_eq!(f.work_status().last().unwrap()["workActivity"]["status"], "completed");
    session.lock_state().activities.add(&json!({ "kind": "message", "text": "long private progress prose", "eventKey": "m1" }));
    progress::publish(f.shared(), &session);
    assert_eq!(f.work_status().last().unwrap()["workActivity"], json!({ "kind": "message", "text": "", "status": "completed" }));
    let count = f.work_status().len();
    session.lock_state().activities.add(&json!({ "kind": "message", "text": "different prose", "eventKey": "m2" }));
    progress::publish(f.shared(), &session);
    assert_eq!(f.work_status().len(), count, "응답 글은 같은 상태를 다시 내보내지 않는다");
    f.manager.close("boss").await.unwrap();
}

#[tokio::test]
async fn status_obeys_completion_lock_and_includes_fresh_turn_identity() {
    let f = progress_fixture().await;
    let session = f.session();
    {
        let mut state = session.lock_state();
        state.activities.add(&json!({ "kind": "message", "text": "진행", "eventKey": "m" }));
        state.tool_events_closing = true;
    }
    progress::publish(f.shared(), &session);
    assert!(f.work_status().is_empty());
    session.lock_state().tool_events_closing = false;
    progress::publish(f.shared(), &session);
    {
        let mut state = session.lock_state();
        state.running_turn_id = Some("turn-2".into());
        state.activities = TerminalActivityCollector::default();
        state.collector_generation += 1;
        state.activities.add(&json!({ "kind": "message", "text": "다음 턴", "eventKey": "n" }));
    }
    progress::publish(f.shared(), &session);
    let turns: Vec<Value> = f.work_status().iter().map(|event| event["turnId"].clone()).collect();
    assert_eq!(turns, [json!("turn-1"), json!("turn-2")]);
    assert_eq!(session.lock_state().tool_activity_turn, None);
    f.manager.close("boss").await.unwrap();
}

/// claudeFixture. 지난 답변 한 줄 뒤를 시작 위치로 잡는다.
async fn claude_progress_fixture() -> (Fixture, String, ClaudeTranscript) {
    let f = progress_fixture().await;
    let directory = f.home.join(&["live"]);
    fs::create_dir_all(&directory).unwrap();
    let path = join_path(&[&directory, "session.jsonl"]);
    fs::write(&path, lines(&[json!({ "type": "assistant", "message": { "content": [{ "type": "text", "text": "과거 답변" }] } })]))
        .unwrap();
    let metadata = fs::metadata(&path).unwrap();
    let transcript = ClaudeTranscript {
        path: Some(path.clone()),
        offset: Some(metadata.len()),
        inode: Some(metadata.ino()),
        started_at: "2026-10-02T00:00:00Z".into(),
    };
    f.session().lock_state().claude_transcript = Some(transcript.clone());
    (f, path, transcript)
}

fn transcript_row(text: &str, extra: Value) -> Value {
    with(
        json!({ "type": "assistant", "sessionId": "session-1", "timestamp": "2026-10-02T01:00:00Z",
            "message": { "id": text, "content": [{ "type": "text", "text": text }] } }),
        extra,
    )
}

#[tokio::test]
async fn claude_imports_new_same_session_public_events_and_waits_for_complete_utf8_lines() {
    let (f, path, transcript) = claude_progress_fixture().await;
    let session = f.session();
    let mut cursor = Cursor::new("turn-1".into(), transcript).unwrap();
    let text = lines(&[
        transcript_row("정상 진행", json!({})),
        transcript_row("다른 세션", json!({ "sessionId": "other" })),
        transcript_row("다른 작업자", json!({ "isSidechain": true })),
        transcript_row("예전 시각", json!({ "timestamp": "2026-10-01T01:00:00Z" })),
    ]);
    let bytes = text.as_bytes();
    // "정" 첫 바이트 바로 뒤에서 자른다.
    let split = text.find("정상").unwrap() + 1;
    append_bytes(&path, &bytes[..split]);
    cursor.sweep(f.shared(), &session).unwrap();
    assert!(f.work_status().is_empty());
    append_bytes(&path, &bytes[split..]);
    cursor.sweep(f.shared(), &session).unwrap();
    let texts: Vec<Value> = session.lock_state().activities.snapshot("", false).iter().map(|activity| activity["text"].clone()).collect();
    assert_eq!(texts, [json!("정상 진행")]);
    assert_eq!(f.work_status().last().unwrap()["workActivity"], json!({ "kind": "message", "text": "", "status": "completed" }));
    f.manager.close("boss").await.unwrap();
}

/// Node는 멈춘 감시기의 sweep을 불렀다. Rust는 멈추면 커서도 사라지므로 debounce보다 오래 기다려 새 알림이 없음을 본다.
#[tokio::test]
async fn claude_file_events_publish_before_stop_and_stopped_watcher_reads_nothing() {
    let (f, path, _) = claude_progress_fixture().await;
    let session = f.session();
    let watcher = ProgressWatcher::start(f.shared(), &session).expect("폴더 감시");
    append(&path, &lines(&[transcript_row("작업 중", json!({}))]));
    wait_until(|| !f.work_status().is_empty()).await;
    watcher.stop().await;
    let count = f.work_status().len();
    append(&path, &lines(&[transcript_row("종료 이후", json!({}))]));
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(f.work_status().len(), count);
    f.manager.close("boss").await.unwrap();
}

#[tokio::test]
async fn claude_stops_live_reads_after_turn_change_or_file_truncation() {
    let (f, path, transcript) = claude_progress_fixture().await;
    let session = f.session();
    let mut cursor = Cursor::new("turn-1".into(), transcript).unwrap();
    fs::write(&path, "\n").unwrap();
    cursor.sweep(f.shared(), &session).unwrap();
    assert!(f.work_status().is_empty());
    session.lock_state().running_turn_id = Some("turn-2".into());
    append(&path, &lines(&[transcript_row("섞으면 안 됨", json!({}))]));
    cursor.sweep(f.shared(), &session).unwrap();
    assert!(f.work_status().is_empty());
    f.manager.close("boss").await.unwrap();
}
