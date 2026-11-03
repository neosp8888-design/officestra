// 실행 중인 턴 하나의 상태와 CLI 출력 처리. Node agent-runtime.mjs의 state 객체와 consumeOutputLine·addActivity·
// persistUsageRecord 묶음을 옮겼다. 한 턴의 출력·감시·중단은 상태 잠금으로 차례를 지킨다(Node 이벤트 루프와 같은 효과).

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use serde_json::{Map, Value, json};
use sqlx::PgConnection;

use super::attachments::StagedAttachment;
use super::file_changes::{FileSnapshots, capture_file_snapshots, file_change_statistics};
use super::launch::RuntimeCharacter;
use super::process_group::ProcessGroup;
use super::rollout::{RolloutReader, find_rollout_path};
use super::service::RuntimeServices;
use super::usage::{accumulated_usage, usage_for_turn};
use crate::agent_event_parser::{decode_agent_response, file_change_activity_text, parse_agent_event};
use crate::antigravity_local_state::antigravity_turn_request_usages;
use crate::antigravity_reasoning::antigravity_step_reasonings;
use crate::claude_persistent_worker::ClaudePersistentWorker;
use crate::developer_tool_events::DeveloperToolEventKeys;
use crate::js::{is_js_integer, js_join, js_string, js_trim, js_truthy, now_milliseconds, number_to_string, pg_text, to_js_json};
use crate::structured_turn_result::{apply_structured_turn_result, consume_structured_turn_result};
use crate::terminal_usage::{CodexScope, TurnWindow, codex_rollout_turn_usage};
use crate::token_cost_estimator::{CostQuery, estimate_turn_token_cost};

/// 실행 중 잡 표에 들어가는 턴. 중단 요청과 프로세스는 상태 잠금 없이도 닿아야 해서 따로 둔다.
pub struct TurnHandle {
    pub turn_id: String,
    pub character_id: String,
    pub(crate) cancel_requested: Arc<AtomicBool>,
    pub(crate) process: Mutex<Option<ProcessGroup>>,
    pub(crate) claude_worker: Mutex<Option<Arc<ClaudePersistentWorker>>>,
    pub(crate) state: tokio::sync::Mutex<TurnState>,
}

impl TurnHandle {
    pub fn new(state: TurnState) -> Arc<Self> {
        Arc::new(Self {
            turn_id: state.turn_id.clone(),
            character_id: state.character_id().to_string(),
            cancel_requested: Arc::clone(&state.cancel_requested),
            process: Mutex::new(None),
            claude_worker: Mutex::new(None),
            state: tokio::sync::Mutex::new(state),
        })
    }

    pub fn cancel_requested(&self) -> bool {
        self.cancel_requested.load(Ordering::SeqCst)
    }

    pub(crate) fn set_process(&self, process: Option<ProcessGroup>) {
        *self.process.lock().unwrap_or_else(PoisonError::into_inner) = process;
    }

    pub(crate) fn process(&self) -> Option<ProcessGroup> {
        self.process.lock().unwrap_or_else(PoisonError::into_inner).clone()
    }

    pub(crate) fn set_claude_worker(&self, worker: Option<Arc<ClaudePersistentWorker>>) {
        *self.claude_worker.lock().unwrap_or_else(PoisonError::into_inner) = worker;
    }

    pub(crate) fn claude_worker(&self) -> Option<Arc<ClaudePersistentWorker>> {
        self.claude_worker.lock().unwrap_or_else(PoisonError::into_inner).clone()
    }
}

#[derive(Debug, Clone)]
struct ActivityRecord {
    sequence: i32,
    kind: Value,
    text: Value,
    status: Value,
    collaboration: Value,
}

#[derive(Debug, Clone, PartialEq)]
struct AgentMessage {
    key: Option<String>,
    text: String,
}

/// Node state. prepareTurn 결과에 실행 중 출력 처리 값이 더해진다.
pub struct TurnState {
    pub turn_id: String,
    pub session_id: String,
    pub conversation_id: String,
    pub external_session_id: Option<String>,
    pub resume_execution_workdir: Option<String>,
    pub character: RuntimeCharacter,
    pub prompt: String,
    pub record_prompt: String,
    pub execution_prompt: String,
    pub workdir: String,
    pub structured_result_path: Option<String>,
    pub attachments: Vec<StagedAttachment>,
    pub reply_recipient_ids: Vec<String>,
    pub usage_started_at: f64,
    pub usage_rollout_offset: u64,
    pub initial_generated_images: HashSet<String>,
    pub resumed_codex_session: bool,
    pub resumed_antigravity_session: bool,
    pub antigravity_reasoning_baseline: Option<f64>,
    pub usage_baseline: Option<Value>,
    pub usage: Option<Value>,
    pub warning: Option<String>,
    pub failure: Option<String>,
    pub resolved_model: Option<String>,
    /// state.endedAt. 터미널 턴만 채운다.
    pub ended_at: Option<f64>,
    /// state.refreshTerminalResult. Antigravity 터미널의 같은 질문 후속 응답으로 완료 턴을 다시 저장한다.
    pub refresh_terminal_result: bool,
    pub developer_tool_event_keys: DeveloperToolEventKeys,
    pub(crate) cancel_requested: Arc<AtomicBool>,
    pub(crate) rollout_reader: Option<RolloutReader>,
    sequence: i32,
    last_activity: Option<String>,
    activity_records: HashMap<String, ActivityRecord>,
    file_change_snapshots: HashMap<String, FileSnapshots>,
    emitted_reasoning: HashMap<String, (String, &'static str)>,
    has_seen_initial_codex_reasoning: bool,
    pending_initial_codex_reasoning: Option<Value>,
    pending_agent_message: Option<AgentMessage>,
    visible_agent_messages: Vec<AgentMessage>,
    stream_message_id: Option<String>,
    response_step_key: Option<String>,
    pub(crate) response_text: String,
    pub(crate) partial_text: String,
    last_partial_persisted_at: f64,
}

/// TurnState를 만들 때 prepareTurn·startAccepted가 정하는 값.
pub struct TurnSeed {
    pub turn_id: String,
    pub session_id: String,
    pub conversation_id: String,
    pub external_session_id: Option<String>,
    pub resume_execution_workdir: Option<String>,
    pub character: RuntimeCharacter,
    pub prompt: String,
    pub execution_prompt: String,
    pub workdir: String,
    pub structured_result_path: Option<String>,
    pub attachments: Vec<StagedAttachment>,
    pub reply_recipient_ids: Vec<String>,
}

impl TurnState {
    pub fn new(seed: TurnSeed) -> Self {
        Self {
            turn_id: seed.turn_id,
            session_id: seed.session_id,
            conversation_id: seed.conversation_id,
            external_session_id: seed.external_session_id,
            resume_execution_workdir: seed.resume_execution_workdir,
            character: seed.character,
            record_prompt: seed.prompt.clone(),
            prompt: seed.prompt,
            execution_prompt: seed.execution_prompt,
            workdir: seed.workdir,
            structured_result_path: seed.structured_result_path,
            attachments: seed.attachments,
            reply_recipient_ids: seed.reply_recipient_ids,
            usage_started_at: now_milliseconds(),
            usage_rollout_offset: 0,
            initial_generated_images: HashSet::new(),
            resumed_codex_session: false,
            resumed_antigravity_session: false,
            antigravity_reasoning_baseline: None,
            usage_baseline: None,
            usage: None,
            warning: None,
            failure: None,
            resolved_model: None,
            ended_at: None,
            refresh_terminal_result: false,
            developer_tool_event_keys: DeveloperToolEventKeys::default(),
            cancel_requested: Arc::default(),
            rollout_reader: None,
            sequence: 0,
            last_activity: None,
            activity_records: HashMap::new(),
            file_change_snapshots: HashMap::new(),
            emitted_reasoning: HashMap::new(),
            has_seen_initial_codex_reasoning: false,
            pending_initial_codex_reasoning: None,
            pending_agent_message: None,
            visible_agent_messages: Vec::new(),
            stream_message_id: None,
            response_step_key: None,
            response_text: String::new(),
            partial_text: String::new(),
            last_partial_persisted_at: 0.0,
        }
    }

    pub fn character_id(&self) -> &str {
        self.character.id.as_deref().unwrap_or("")
    }

    pub fn backend(&self) -> &str {
        &self.character.backend
    }

    fn cancel_requested(&self) -> bool {
        self.cancel_requested.load(Ordering::SeqCst)
    }

    fn resumed_cumulative_session(&self) -> bool {
        (self.backend() == "codex" && self.resumed_codex_session)
            || (self.backend() == "antigravity" && self.resumed_antigravity_session)
    }

    /// usageForTurn.
    pub fn usage_for_turn(&self, usage: Value) -> Option<Value> {
        usage_for_turn(self.resumed_cumulative_session(), self.usage_baseline.as_ref(), usage)
    }

    fn feed_changed(&self, services: &RuntimeServices) {
        services.broadcast(json!({ "type": "feed.changed", "turnId": self.turn_id, "characterId": self.character_id() }));
    }

    /// consumeOutputLine.
    pub async fn consume_output_line(&mut self, services: &RuntimeServices, line: &str) -> Result<(), String> {
        let Some(mut event) = parse_agent_event(line, &self.character.backend, Some(&self.workdir))? else {
            return Ok(());
        };
        if let Some(usage) = event.get("usage").filter(|usage| js_truthy(Some(usage))).cloned()
            && let Some(usage) = self.usage_for_turn(usage)
        {
            self.usage = if js_truthy(event.get("usageIsDelta")) {
                accumulated_usage(self.usage.as_ref(), Some(&usage))
            } else {
                Some(usage)
            };
        }
        if let Some(fallback) = event.get("usageFallback").filter(|usage| js_truthy(Some(usage))).cloned()
            && self.usage.is_none()
        {
            self.usage = self.usage_for_turn(fallback);
        }
        self.enrich_file_change_event(&mut event);
        if let Some(session_id) = truthy_text(event.get("sessionID"))
            && Some(&session_id) != self.external_session_id.as_ref()
        {
            self.activate_session(services, &session_id).await?;
        }
        if let Some(model) = truthy_text(event.get("resolvedModel")) {
            self.record_resolved_model(services, &model).await?;
        }
        if let Some(message_id) = truthy_text(event.get("streamMessageID")) {
            self.stream_message_id = Some(message_id);
            self.partial_text.clear();
            self.response_text.clear();
            self.last_partial_persisted_at = 0.0;
        }
        let mut activities: Vec<Value> = event.get("activities").and_then(Value::as_array).cloned().unwrap_or_default();
        if let Some(activity) = event.get("activity").filter(|activity| js_truthy(Some(activity))) {
            activities.push(activity.clone());
        }
        let pending_reasoning_before_event =
            if activities.is_empty() { None } else { self.pending_initial_codex_reasoning.clone() };
        if !activities.is_empty() {
            // Codex 메시지 뒤에 추론·도구·명령이 또 오면 그 메시지는 진행 안내라 최종 답 후보에서 뺀다.
            if self.backend() == "codex" && !js_trim(&self.response_text).is_empty() {
                self.response_text.clear();
                self.partial_text.clear();
            }
            self.promote_pending_agent_message(services).await?;
            for activity in activities {
                let scoped = self.scoped_activity(activity);
                self.add_parsed_activity(services, scoped).await?;
            }
            if let Some(pending) = pending_reasoning_before_event {
                self.complete_pending_initial_codex_reasoning(services, Some(&pending)).await?;
            }
        }
        if let Some(message) = truthy_text(event.get("agentMessage")) {
            let key = event.get("agentMessageKey").filter(|key| !key.is_null()).map(js_string);
            self.record_agent_message(services, key, message).await?;
        }
        // Antigravity는 응답 단계마다 조각을 보낸다. 새 단계가 오면 앞 단계 조각을 비워 중간 메시지가 이어 붙지 않게 한다.
        if let Some(step_key) = truthy_text(event.get("responseStepKey"))
            && Some(&step_key) != self.response_step_key.as_ref()
        {
            self.response_step_key = Some(step_key);
            self.partial_text.clear();
        }
        if let Some(delta) = truthy_text(event.get("responseDelta")) {
            self.partial_text.push_str(&delta);
            self.persist_partial_response(services).await?;
        }
        if js_truthy(event.get("responseStepDone")) && !js_trim(&self.partial_text).is_empty() {
            let key = event.get("responseStepKey").filter(|key| !key.is_null()).map(js_string);
            let text = js_trim(&self.partial_text).to_string();
            self.record_agent_message(services, key, text).await?;
        }
        let response_text = match event.get("responseText").filter(|text| !text.is_null()) {
            Some(text) => Some(text),
            None if self.visible_agent_messages.is_empty() => event.get("responseFallback"),
            None => None,
        };
        if let Some(text) = truthy_text(response_text) {
            self.remember_visible_agent_message(None, &text);
            self.response_text = text;
            let visible = self.visible_response_text("");
            self.persist_response_draft(services, &visible).await?;
        }
        if let Some(warning) = truthy_text(event.get("warning")) {
            self.warning = Some(warning);
        }
        if let Some(failure) = truthy_text(event.get("failure")) {
            self.failure = Some(failure);
        }
        Ok(())
    }

    /// scopedActivity. 메시지 안에서만 유일한 열쇠는 스트림 메시지 ID를 앞에 붙인다.
    fn scoped_activity(&self, activity: Value) -> Value {
        let Some(message_id) = self.stream_message_id.as_deref().filter(|_| js_truthy(activity.get("messageScoped"))) else {
            return activity;
        };
        let mut scoped = activity;
        if let Value::Object(fields) = &mut scoped {
            let key = format!("{message_id}:{}", template(fields.get("eventKey")));
            fields.insert("eventKey".into(), Value::String(key));
            fields.insert("messageScoped".into(), Value::Bool(false));
        }
        scoped
    }

    /// enrichFileChangeEvent. 시작 때 파일을 찍어 두고, 끝나면 rollout 통계나 직접 비교로 줄 수를 붙인다.
    fn enrich_file_change_event(&mut self, event: &mut Value) {
        let Some(file_change) = event.get("fileChange") else {
            return;
        };
        let Some(event_key) = truthy_text(file_change.get("eventKey")) else {
            return;
        };
        let changes: Vec<Value> = file_change.get("changes").and_then(Value::as_array).cloned().unwrap_or_default();
        match file_change.get("phase").and_then(Value::as_str) {
            Some("item.started") => {
                self.file_change_snapshots.insert(event_key, capture_file_snapshots(&self.workdir, &changes));
            }
            Some("item.completed") => {
                let snapshots = self.file_change_snapshots.remove(&event_key);
                let workdir = self.workdir.clone();
                let statistics = self
                    .rollout_reader
                    .as_mut()
                    .and_then(|reader| reader.file_change_statistics(&workdir, &changes))
                    .or_else(|| file_change_statistics(&workdir, snapshots.as_ref(), &changes));
                let raw_changes = event["fileChange"].get("changes").cloned();
                if let Some(statistics) = statistics
                    && let Some(Value::Object(activity)) = event.get_mut("activity")
                {
                    let text = file_change_activity_text(raw_changes.as_ref(), Some(&statistics), Some(&workdir));
                    activity.insert("text".into(), Value::String(text));
                }
            }
            _ => {}
        }
    }

    /// addParsedActivity. 첫 Codex 추론이 완료로 오면 먼저 진행 중으로 보이고, 다음 사건 뒤에 완료로 바꾼다.
    async fn add_parsed_activity(&mut self, services: &RuntimeServices, activity: Value) -> Result<(), String> {
        let initial = activity.get("isCodexReasoning") == Some(&Value::Bool(true)) && !self.has_seen_initial_codex_reasoning;
        if !initial {
            return self.add_activity(services, &activity).await;
        }
        self.has_seen_initial_codex_reasoning = true;
        if activity.get("status").and_then(Value::as_str) != Some("completed") || !js_truthy(activity.get("eventKey")) {
            return self.add_activity(services, &activity).await;
        }
        let mut running = activity.clone();
        set_field(&mut running, "status", json!("running"));
        self.add_activity(services, &running).await?;
        let mut pending = activity;
        set_field(&mut pending, "status", json!("completed"));
        set_field(&mut pending, "preserveOccurredAt", json!(true));
        self.pending_initial_codex_reasoning = Some(pending);
        Ok(())
    }

    /// completePendingInitialCodexReasoning.
    pub async fn complete_pending_initial_codex_reasoning(
        &mut self,
        services: &RuntimeServices,
        expected: Option<&Value>,
    ) -> Result<(), String> {
        let Some(pending) = self.pending_initial_codex_reasoning.as_ref() else {
            return Ok(());
        };
        if let Some(expected) = expected
            && pending.get("eventKey") != expected.get("eventKey")
        {
            return Ok(());
        }
        let pending = self.pending_initial_codex_reasoning.take().unwrap_or_default();
        if let Err(error) = self.add_activity(services, &pending).await {
            self.pending_initial_codex_reasoning.get_or_insert(pending);
            return Err(error);
        }
        Ok(())
    }

    /// rememberVisibleAgentMessage.
    fn remember_visible_agent_message(&mut self, key: Option<&str>, text: &str) {
        let value = js_trim(text).to_string();
        if value.is_empty() {
            return;
        }
        if let Some(key) = key.filter(|key| !key.is_empty()) {
            let message = AgentMessage { key: Some(key.to_string()), text: value };
            match self.visible_agent_messages.iter_mut().find(|message| message.key.as_deref() == Some(key)) {
                Some(existing) => *existing = message,
                None => self.visible_agent_messages.push(message),
            }
            return;
        }
        if self.visible_agent_messages.last().map(|message| message.text.as_str()) != Some(value.as_str()) {
            self.visible_agent_messages.push(AgentMessage { key: key.map(str::to_string), text: value });
        }
    }

    /// visibleResponseText.
    fn visible_response_text(&self, current: &str) -> String {
        let mut values: Vec<&str> =
            self.visible_agent_messages.iter().map(|message| message.text.as_str()).filter(|text| !text.is_empty()).collect();
        let trimmed = js_trim(current);
        if !trimmed.is_empty() && values.last() != Some(&trimmed) {
            values.push(current);
        }
        values.join("\n\n")
    }

    /// finalResponseCandidate.
    pub fn final_response_candidate(&self) -> String {
        let mut candidates = vec![self.response_text.as_str(), self.partial_text.as_str()];
        if self.backend() != "codex"
            && let Some(last) = self.visible_agent_messages.last()
        {
            candidates.push(&last.text);
        }
        candidates.into_iter().find(|value| !js_trim(value).is_empty()).unwrap_or("").to_string()
    }

    /// decodeCompletedResponse. 구조화 결과 파일은 읽은 뒤 지운다.
    pub fn decode_completed_response(&mut self, candidate: &str) -> Value {
        let decoded = decode_agent_response(candidate);
        let structured = consume_structured_turn_result(self.structured_result_path.as_deref());
        self.structured_result_path = None;
        apply_structured_turn_result(&decoded, Some(&structured))
    }

    /// completedResponseText. 조각마다 기계 블록을 뗀 뒤 합친다.
    pub fn completed_response_text(&self, decoded: &Value) -> String {
        let decoded_text = decoded.get("text").map(js_string).unwrap_or_default();
        let mut values: Vec<String> = self
            .visible_agent_messages
            .iter()
            .map(|message| decode_agent_response(&message.text).get("text").map(js_string).unwrap_or_default())
            .filter(|text| !text.is_empty())
            .collect();
        if values.last() != Some(&decoded_text) {
            values.push(decoded_text);
        }
        values.join("\n\n")
    }

    /// normalizeCompletedMessageActivity. 마지막 공개 메시지 활동을 기계 블록을 뗀 본문으로 맞춘다.
    pub async fn normalize_completed_message_activity(
        &mut self,
        services: &RuntimeServices,
        decoded: &Value,
    ) -> Result<(), String> {
        let decoded_text = decoded.get("text").map(js_string).unwrap_or_default();
        let Some(final_message) = self.visible_agent_messages.last().cloned() else {
            return Ok(());
        };
        let Some(key) = final_message.key.as_deref().filter(|key| !key.is_empty()) else {
            return Ok(());
        };
        let event_key = format!("message:{key}");
        if !self.activity_records.contains_key(&event_key) || final_message.text == decoded_text {
            return Ok(());
        }
        let activity = json!({
            "kind": "message",
            "text": decoded_text,
            "eventKey": event_key,
            "status": "completed",
            "preserveOccurredAt": true,
        });
        self.add_activity(services, &activity).await?;
        if let Some(last) = self.visible_agent_messages.last_mut() {
            last.text = decoded_text.clone();
        }
        if self.response_text == final_message.text {
            self.response_text = decoded_text.clone();
        }
        if self.partial_text == final_message.text {
            self.partial_text = decoded_text;
        }
        Ok(())
    }

    /// recordAgentMessage. Codex는 바로 메시지 활동이 되고, 다른 CLI는 다음 활동이 올 때까지 대기한다.
    async fn record_agent_message(&mut self, services: &RuntimeServices, key: Option<String>, text: String) -> Result<(), String> {
        if self.backend() == "codex" {
            self.complete_pending_initial_codex_reasoning(services, None).await?;
            let activity = json!({
                "kind": "message",
                "text": text,
                "eventKey": message_event_key(key.as_deref()),
                "status": "completed",
                "preserveOccurredAt": true,
            });
            self.add_activity(services, &activity).await?;
        } else {
            if self.pending_agent_message.as_ref().is_some_and(|pending| pending.key != key) {
                self.promote_pending_agent_message(services).await?;
            }
            self.pending_agent_message = Some(AgentMessage { key: key.clone(), text: text.clone() });
        }
        self.remember_visible_agent_message(key.as_deref(), &text);
        self.response_text = text.clone();
        self.partial_text = text;
        let visible = self.visible_response_text("");
        self.persist_response_draft(services, &visible).await
    }

    /// promotePendingAgentMessage.
    async fn promote_pending_agent_message(&mut self, services: &RuntimeServices) -> Result<(), String> {
        let Some(pending) = self.pending_agent_message.take() else {
            return Ok(());
        };
        let activity = json!({
            "kind": "message",
            "text": pending.text,
            "eventKey": message_event_key(pending.key.as_deref()),
            "status": "completed",
            "preserveText": false,
        });
        self.add_activity(services, &activity).await?;
        if self.response_text == pending.text {
            self.response_text.clear();
            self.partial_text.clear();
        }
        Ok(())
    }

    /// completeTerminalTurn의 이전 활동 복원. 순번은 가장 큰 값으로 올리고, 열쇠 있는 활동만 다시 고쳐 쓸 수 있게 기억한다.
    pub(crate) fn restore_activity(
        &mut self,
        sequence: i32,
        event_key: Option<String>,
        kind: Value,
        text: Value,
        status: Value,
        collaboration: Value,
    ) {
        self.sequence = self.sequence.max(sequence);
        if let Some(event_key) = event_key.filter(|key| !key.is_empty()) {
            self.activity_records.insert(event_key, ActivityRecord { sequence, kind, text, status, collaboration });
        }
    }

    /// recordResolvedModel. CLI가 실제로 답한 모델 ID를 남긴다. 요청값(model)은 재실행용으로 그대로 둔다.
    pub(crate) async fn record_resolved_model(&mut self, services: &RuntimeServices, model: &str) -> Result<(), String> {
        if self.resolved_model.as_deref() == Some(model) {
            return Ok(());
        }
        self.resolved_model = Some(model.to_string());
        sqlx::query("UPDATE turns SET resolved_model = $2 WHERE id = $1::uuid AND resolved_model IS DISTINCT FROM $2")
            .bind(&self.turn_id)
            .bind(model)
            .execute(&services.pool)
            .await
            .map_err(|error| database_error(&error))?;
        Ok(())
    }

    /// activateSession. CLI가 알려 준 세션 ID를 저장하고 이 세션을 직원의 활성 세션으로 바꾼다.
    async fn activate_session(&mut self, services: &RuntimeServices, external_session_id: &str) -> Result<(), String> {
        let is_new_session = self.external_session_id.is_none();
        let character_id = self.character_id().to_string();
        let mut transaction = services.pool.begin().await.map_err(|error| database_error(&error))?;
        let statements = async {
            sqlx::query(
                "UPDATE cli_sessions SET external_id = $2 WHERE id = $1::uuid AND (external_id IS NULL OR external_id = $2)",
            )
            .bind(&self.session_id)
            .bind(external_session_id)
            .execute(&mut *transaction)
            .await?;
            sqlx::query(
                "UPDATE cli_sessions SET ended_at = COALESCE(ended_at, now())
                 WHERE character_id = $1 AND id <> $2::uuid AND ended_at IS NULL",
            )
            .bind(&character_id)
            .bind(&self.session_id)
            .execute(&mut *transaction)
            .await?;
            sqlx::query(
                "INSERT INTO active_cli_sessions (character_id, cli_session_id) VALUES ($1, $2::uuid)
                 ON CONFLICT (character_id) DO UPDATE SET
                   cli_session_id = EXCLUDED.cli_session_id,
                   activated_at = CASE WHEN active_cli_sessions.cli_session_id = EXCLUDED.cli_session_id
                     THEN active_cli_sessions.activated_at ELSE now() END,
                   updated_at = now()",
            )
            .bind(&character_id)
            .bind(&self.session_id)
            .execute(&mut *transaction)
            .await?;
            Ok::<_, sqlx::Error>(())
        };
        statements.await.map_err(|error| database_error(&error))?;
        transaction.commit().await.map_err(|error| database_error(&error))?;
        self.external_session_id = Some(external_session_id.to_string());
        self.rollout_reader = RolloutReader::open(&services.roots.codex_sessions(), external_session_id, !is_new_session);
        services.broadcast(json!({ "type": "session.changed", "turnId": self.turn_id }));
        Ok(())
    }

    /// addActivity·persistActivity. 같은 열쇠의 활동은 고쳐 쓰고, 열쇠 없는 같은 활동이 연달아 오면 한 번만 남긴다.
    pub async fn add_activity(&mut self, services: &RuntimeServices, activity: &Value) -> Result<(), String> {
        let event_key = activity.get("eventKey").filter(|key| js_truthy(Some(key))).map(js_string);
        let existing = event_key.as_ref().and_then(|key| self.activity_records.get(key)).cloned();
        let kind = activity.get("kind").cloned().unwrap_or(Value::Null);
        let text = match &existing {
            Some(existing) if js_truthy(activity.get("preserveText")) => existing.text.clone(),
            _ => activity.get("text").cloned().unwrap_or(Value::Null),
        };
        let status = activity.get("status").filter(|status| !status.is_null()).cloned().unwrap_or(json!("completed"));
        let collaboration = activity.get("collaboration").cloned().unwrap_or(Value::Null);
        let collaboration_key = json_text(&collaboration);
        if let Some(existing) = &existing
            && existing.kind == kind
            && existing.text == text
            && existing.status == status
            && json_text(&existing.collaboration) == collaboration_key
        {
            return Ok(());
        }
        let duplicate_key = js_join(&[kind.clone(), text.clone(), status.clone(), Value::String(collaboration_key)], "\n");
        if event_key.is_none() && self.last_activity.as_deref() == Some(duplicate_key.as_str()) {
            return Ok(());
        }
        self.last_activity = Some(duplicate_key);

        if let (Some(existing), Some(event_key)) = (existing, event_key.as_ref()) {
            sqlx::query(
                "UPDATE turn_activities SET kind = $3, text = $4, status = $5, collaboration = $6::jsonb
                 WHERE turn_id = $1::uuid AND seq = $2",
            )
            .bind(&self.turn_id)
            .bind(existing.sequence)
            .bind(pg_text(&kind))
            .bind(pg_text(&text))
            .bind(pg_text(&status))
            .bind(pg_text(&collaboration))
            .execute(&services.pool)
            .await
            .map_err(|error| database_error(&error))?;
            self.activity_records.insert(
                event_key.clone(),
                ActivityRecord { sequence: existing.sequence, kind, text, status, collaboration },
            );
            self.touch_turn(services).await?;
            self.feed_changed(services);
            return Ok(());
        }

        let sequence = self.sequence + 1;
        sqlx::query(
            "INSERT INTO turn_activities (turn_id, seq, kind, text, event_key, status, collaboration)
             VALUES ($1::uuid, $2, $3, $4, $5, $6, $7::jsonb)
             ON CONFLICT (turn_id, seq) DO NOTHING",
        )
        .bind(&self.turn_id)
        .bind(sequence)
        .bind(pg_text(&kind))
        .bind(pg_text(&text))
        .bind(activity.get("eventKey").and_then(pg_text))
        .bind(pg_text(&status))
        .bind(pg_text(&collaboration))
        .execute(&services.pool)
        .await
        .map_err(|error| database_error(&error))?;
        self.sequence = sequence;
        if let Some(event_key) = event_key {
            self.activity_records.insert(event_key, ActivityRecord { sequence, kind, text, status, collaboration });
        }
        self.touch_turn(services).await?;
        self.feed_changed(services);
        Ok(())
    }

    /// persistPartialResponse. 조각 응답은 250ms에 한 번만 저장한다.
    async fn persist_partial_response(&mut self, services: &RuntimeServices) -> Result<(), String> {
        let now = now_milliseconds();
        if now - self.last_partial_persisted_at < 250.0 {
            return Ok(());
        }
        self.last_partial_persisted_at = now;
        let visible = self.visible_response_text(&self.partial_text);
        self.persist_response_draft(services, &visible).await
    }

    /// persistResponseDraft. 화면에는 기계 블록을 뗀 본문만 보낸다.
    async fn persist_response_draft(&self, services: &RuntimeServices, text: &str) -> Result<(), String> {
        let visible = decode_agent_response(text).get("text").map(js_string).unwrap_or_default();
        sqlx::query("UPDATE messages SET text = $2, received_at = now() WHERE turn_id = $1::uuid AND role = 'assistant'")
            .bind(&self.turn_id)
            .bind(visible)
            .execute(&services.pool)
            .await
            .map_err(|error| database_error(&error))?;
        self.touch_turn(services).await?;
        self.feed_changed(services);
        Ok(())
    }

    async fn touch_turn(&self, services: &RuntimeServices) -> Result<(), String> {
        sqlx::query("UPDATE turns SET updated_at = now() WHERE id = $1::uuid")
            .bind(&self.turn_id)
            .execute(&services.pool)
            .await
            .map_err(|error| database_error(&error))?;
        Ok(())
    }

    /// finalizeRunningActivities. 진행 중으로 남은 활동을 끝 상태로 맞춘다. 협업 카드는 agentStatus도 바꾼다.
    pub async fn finalize_running_activities(&mut self, services: &RuntimeServices, status: &str) -> Result<(), String> {
        let agent_status = if status == "completed" { "completed" } else { "errored" };
        sqlx::query(
            "UPDATE turn_activities SET status = $2,
               collaboration = CASE WHEN kind = 'collaboration' AND jsonb_typeof(collaboration) = 'object'
                 THEN jsonb_set(collaboration, '{agentStatus}', to_jsonb($3::text), true)
                 ELSE collaboration END
             WHERE turn_id = $1::uuid AND status = 'running'",
        )
        .bind(&self.turn_id)
        .bind(status)
        .bind(agent_status)
        .execute(&services.pool)
        .await
        .map_err(|error| database_error(&error))?;
        for record in self.activity_records.values_mut() {
            if record.status.as_str() != Some("running") {
                continue;
            }
            record.status = json!(status);
            if record.kind.as_str() == Some("collaboration") {
                let mut fields = match &record.collaboration {
                    Value::Object(fields) => fields.clone(),
                    // JS 전개 연산자는 배열을 번호 열쇠의 객체로 바꾼다.
                    Value::Array(items) => items.iter().enumerate().map(|(index, item)| (index.to_string(), item.clone())).collect(),
                    _ => continue,
                };
                fields.insert("agentStatus".into(), json!(agent_status));
                record.collaboration = Value::Object(fields);
            }
        }
        Ok(())
    }

    /// settleCancelledOutput. 중단된 턴이면 진행 중 활동을 실패로 닫고 true다.
    pub async fn settle_cancelled_output(&mut self, services: &RuntimeServices) -> Result<bool, String> {
        if !self.cancel_requested() {
            return Ok(false);
        }
        self.finalize_running_activities(services, "failed").await?;
        self.feed_changed(services);
        Ok(true)
    }

    /// consumeCodexRolloutActivities. rollout에만 남는 협업(하위 에이전트) 기록을 활동으로 올린다.
    pub async fn consume_codex_rollout_activities(&mut self, services: &RuntimeServices) -> Result<(), String> {
        if self.backend() != "codex" {
            return Ok(());
        }
        if self.rollout_reader.is_none()
            && let Some(session_id) = self.external_session_id.clone()
        {
            self.rollout_reader =
                RolloutReader::open(&services.roots.codex_sessions(), &session_id, self.resumed_codex_session);
        }
        let workdir = self.workdir.clone();
        let Some(reader) = self.rollout_reader.as_mut() else {
            return Ok(());
        };
        let activities = reader.collaboration_activities(&workdir);
        if activities.is_empty() {
            return Ok(());
        }
        self.promote_pending_agent_message(services).await?;
        for activity in activities {
            self.add_parsed_activity(services, activity.clone()).await?;
            if let Some(reader) = self.rollout_reader.as_mut() {
                reader.acknowledge(&activity);
            }
        }
        Ok(())
    }

    /// consumeAntigravityReasoning. agy는 추론을 stdout으로 보내지 않아 대화 SQLite에서 단계별로 읽는다.
    pub async fn consume_antigravity_reasoning(&mut self, services: &RuntimeServices, final_: bool) -> Result<(), String> {
        if self.backend() != "antigravity" {
            return Ok(());
        }
        let Some(conversation_id) = self.external_session_id.clone() else {
            return Ok(());
        };
        let min_step_index = match self.antigravity_reasoning_baseline {
            Some(baseline) if is_js_integer(baseline) => baseline + 1.0,
            _ => 0.0,
        };
        let reasonings =
            antigravity_step_reasonings(&conversation_id, Some(min_step_index), &services.roots.antigravity_conversations());
        let mut activities = Vec::new();
        let mut latest_step_index: Option<f64> = None;
        for reasoning in reasonings {
            let status = if reasoning.done || final_ { "completed" } else { "running" };
            let step = number_to_string(reasoning.step_index);
            if self.emitted_reasoning.get(&step).is_some_and(|(text, previous)| *text == reasoning.text && *previous == status) {
                continue;
            }
            self.emitted_reasoning.insert(step.clone(), (reasoning.text.clone(), status));
            latest_step_index = Some(latest_step_index.map_or(reasoning.step_index, |latest| latest.max(reasoning.step_index)));
            activities.push(json!({
                "kind": "thinking",
                "text": reasoning.text,
                "eventKey": format!("antigravity:{conversation_id}:{step}:thinking"),
                "status": status,
                "preserveText": false,
                "messageScoped": false,
            }));
        }
        if activities.is_empty() {
            return Ok(());
        }
        // 추론을 단계가 끝날 때 한꺼번에 쓰는 모델은 답변이 먼저 온다. 뒤 단계의 추론일 때만 답변을 활동으로 올린다.
        let pending_step_index =
            antigravity_step_index_from_key(self.pending_agent_message.as_ref().and_then(|message| message.key.as_deref()));
        if pending_step_index.is_none_or(|pending| latest_step_index.is_some_and(|latest| latest > pending)) {
            self.promote_pending_agent_message(services).await?;
        }
        for activity in activities {
            let scoped = self.scoped_activity(activity);
            self.add_parsed_activity(services, scoped).await?;
        }
        Ok(())
    }

    /// persistUsageRecord. 요청별 기록을 붙여 비용을 추정하고 usage_records에 덮어쓴다. Claude는 보고 금액만 쓴다.
    pub async fn persist_usage_record(&self, services: &RuntimeServices, connection: &mut PgConnection) -> Result<(), String> {
        let Some(Value::Object(current)) = self.usage.as_ref() else {
            return Ok(());
        };
        let mut usage: Map<String, Value> = current.clone();
        // 로컬 4090 턴은 클라우드 요청 기록이 없고 비용도 매기지 않는다.
        let local = self.character.local_profile.is_some();
        if !local && !js_truthy(usage.get("requestUsages")) {
            let ended_at = self.ended_at.unwrap_or_else(now_milliseconds);
            let session_id = self.external_session_id.as_deref().unwrap_or("");
            match self.backend() {
                "codex" => {
                    let path = find_rollout_path(&services.roots.codex_sessions(), session_id);
                    let window = TurnWindow { started_at: Some(self.usage_started_at), ended_at: Some(ended_at) };
                    if let Some(recorded) =
                        codex_rollout_turn_usage(path.as_deref(), window, CodexScope::Window, self.usage_rollout_offset)
                    {
                        match recorded.get("requestUsages") {
                            Some(requests) => usage.insert("requestUsages".into(), requests.clone()),
                            None => usage.remove("requestUsages"),
                        };
                    }
                }
                "antigravity" => {
                    let requests = antigravity_turn_request_usages(
                        session_id,
                        &services.roots.antigravity_conversations(),
                        self.antigravity_reasoning_baseline,
                        Some(self.usage_started_at),
                        ended_at,
                    );
                    usage.insert("requestUsages".into(), requests.map_or(Value::Null, Value::Array));
                }
                _ => {}
            }
        }
        let usage = Value::Object(usage);
        let model = self.resolved_model.as_deref().or(self.character.model.as_deref());
        let cost = if local {
            None
        } else {
            estimate_turn_token_cost(
                &services.pricing,
                &CostQuery {
                    backend: Some(self.backend()),
                    model,
                    fast_mode: self.character.fast_mode,
                    usage: Some(&usage),
                    priced_at: Some(self.usage_started_at),
                },
            )
        };
        let count = |field: &str| usage.get(field).and_then(pg_text);
        sqlx::query(
            "INSERT INTO usage_records (turn_id, input_tokens, output_tokens, cached_input_tokens, reasoning_output_tokens,
               cost_usd, cache_write_input_tokens, cache_write_5m_input_tokens, cache_write_1h_input_tokens)
             VALUES ($1::uuid, $2::bigint, $3::bigint, $4::bigint, $5::bigint, $6::numeric, $7::bigint, $8::bigint, $9::bigint)
             ON CONFLICT (turn_id) DO UPDATE SET
               input_tokens = EXCLUDED.input_tokens,
               output_tokens = EXCLUDED.output_tokens,
               cached_input_tokens = EXCLUDED.cached_input_tokens,
               reasoning_output_tokens = EXCLUDED.reasoning_output_tokens,
               cost_usd = EXCLUDED.cost_usd,
               cache_write_input_tokens = EXCLUDED.cache_write_input_tokens,
               cache_write_5m_input_tokens = EXCLUDED.cache_write_5m_input_tokens,
               cache_write_1h_input_tokens = EXCLUDED.cache_write_1h_input_tokens",
        )
        .bind(&self.turn_id)
        .bind(count("inputTokens"))
        .bind(count("outputTokens"))
        .bind(count("cachedInputTokens"))
        .bind(count("reasoningOutputTokens"))
        .bind(cost.map(number_to_string))
        .bind(count("cacheWriteInputTokens"))
        .bind(count("cacheWrite5mInputTokens"))
        .bind(count("cacheWrite1hInputTokens"))
        .execute(&mut *connection)
        .await
        .map_err(|error| database_error(&error))?;
        if local {
            // CLI가 보고한 금액은 클라우드 가격 기준이라 비용으로 쓰지 않고 감사용으로만 남긴다.
            let reported = |field: &str| usage.get(field).cloned().unwrap_or(Value::Null);
            let audit = json!({ "reportedCostUsd": reported("reportedCostUsd"), "reportedSonnet5CostUsd": reported("reportedSonnet5CostUsd") });
            sqlx::query("UPDATE usage_records SET cost_basis='local-not-applicable', reported_cost_audit=$2::jsonb WHERE turn_id=$1::uuid")
                .bind(&self.turn_id)
                .bind(audit)
                .execute(&mut *connection)
                .await
                .map_err(|error| database_error(&error))?;
        }
        Ok(())
    }
}

/// pg 오류는 Node처럼 데이터베이스 문구만 남긴다.
pub fn database_error(error: &sqlx::Error) -> String {
    crate::rag::database_message(error)
}

/// JS 참인 값만 문자열로 꺼낸다. 빈 문자열·null·false·0은 없는 값이다.
fn truthy_text(value: Option<&Value>) -> Option<String> {
    value.filter(|value| js_truthy(Some(value))).map(js_string)
}

/// 템플릿 문자열 `${value}`. 없는 값은 "undefined"다.
fn template(value: Option<&Value>) -> String {
    value.map_or_else(|| "undefined".into(), js_string)
}

fn message_event_key(key: Option<&str>) -> Value {
    key.filter(|key| !key.is_empty()).map_or(Value::Null, |key| Value::String(format!("message:{key}")))
}

fn json_text(value: &Value) -> String {
    String::from_utf8(to_js_json(value).unwrap_or_default()).unwrap_or_default()
}

fn set_field(value: &mut Value, key: &str, field: Value) {
    if let Value::Object(fields) = value {
        fields.insert(key.into(), field);
    }
}

/// antigravityStepIndexFromKey. "antigravity:<대화>:<단계>"에서 단계 번호를 꺼낸다.
pub fn antigravity_step_index_from_key(key: Option<&str>) -> Option<f64> {
    let rest = key?.strip_prefix("antigravity:")?;
    let (conversation, step) = rest.split_once(':')?;
    if conversation.is_empty() || step.is_empty() || !step.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    step.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 단계_열쇠는_node_정규식과_같다() {
        assert_eq!(antigravity_step_index_from_key(Some("antigravity:abc:12")), Some(12.0));
        assert_eq!(antigravity_step_index_from_key(Some("antigravity:abc:007")), Some(7.0));
        assert_eq!(antigravity_step_index_from_key(Some("antigravity:abc:12:thinking")), None);
        assert_eq!(antigravity_step_index_from_key(Some("antigravity::12")), None);
        assert_eq!(antigravity_step_index_from_key(Some("antigravity:abc:")), None);
        assert_eq!(antigravity_step_index_from_key(Some("antigravity:abc:1a")), None);
        assert_eq!(antigravity_step_index_from_key(None), None);
    }
}
