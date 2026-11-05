// 직원 CLI 잡의 생명주기. Node AgentRuntime의 접수·준비·실행·완료·실패·중단·복구와 지속 세션 관리를 옮겼다.
// 한 턴의 상태는 TurnHandle의 상태 잠금으로 차례를 지키고, 잡 표(std Mutex)는 await 너머로 쥐지 않는다.

use std::path::Path;
use std::process::Stdio;
use std::sync::atomic::Ordering;
use std::sync::{Arc, MutexGuard, PoisonError};
use std::time::Duration;

use serde_json::{Value, json};
use sqlx::{AssertSqlSafe, PgConnection, Row};
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::sync::{mpsc, oneshot};
use tokio::time::MissedTickBehavior;

use super::attachments::{prompt_with_attachments, remove_staged_attachments, stage_attachments};
use super::claude_session::{claude_session_path, claude_usage_session_path, prepare_claude_session_resume};
use super::codex_app_server::{self, CodexSteer, CodexTurnRequest, codex_turn_input};
use super::launch::{
    RuntimeCharacter, build_arguments, claude_persistent_arguments, claude_persistent_worker_signature,
    codex_app_server_arguments, execution_environment,
};
use super::locate_executable;
use super::process_group::{ProcessGroup, detached_command};
use super::rollout::{RolloutReader, codex_rollout_size, find_rollout_path};
use super::service::{AgentError, JobTable, RuntimeServices};
use super::turn::{TurnHandle, TurnSeed, TurnState};
use super::usage::{latest_claude_usage_from_session, latest_codex_usage_from_rollout};
use super::workspace::{
    sync_work_record_rag_best_effort, transition_work_record_review_best_effort, workspace_columns, workspace_from_row,
};
use crate::antigravity_local_state::antigravity_session_usage;
use crate::antigravity_reasoning::antigravity_latest_step_index;
use crate::character_settings::CharacterSettingsError;
use crate::claude_persistent_worker::{ClaudePersistentWorker, DEFAULT_STEER_RESULT_TIMEOUT, WorkerOptions};
use crate::db::with_transaction;
use crate::developer_tool_events::parse_developer_tool_event;
use crate::health::{Runtime, SessionEndPlan};
use crate::js::{
    iso_string, join_path, js_length, js_string, js_string_or_empty, js_trim, js_truthy, pg_text, random_uuid,
    realpath_sync, resolve_path, utf16_prefix, utf16_suffix, uv_error,
};
use crate::local_artifacts::{append_local_image_previews, list_generated_images};
use crate::local_agent_profile::LocalAgentLaunch;
use crate::local_embedding::process_environment;
use crate::local_provider::service::LaunchRequest;
use crate::local_provider::{normalize_local_definition, resolve_local_character, snapshot_local_turn, validate_local_resume};
use crate::reply_delivery::{ReplyDeliveryError, record_reply_delivery, reply_routing_prompt};
use crate::structured_turn_result::{discard_structured_turn_result, prepare_structured_turn_result};
use crate::wiki_knowledge::{TurnWikiProposals, persist_turn_wiki_proposals};
use crate::work_record_memory::{CompletedTurnRecord, ReviewTransition, persist_completed_turn_work_record};
use crate::work_record_provenance::{SourceStoreError, portable_response_sources, replace_turn_response_sources};

/// CODEX_ROLLOUT_MONITOR_INTERVAL_MS·ANTIGRAVITY_REASONING_MONITOR_INTERVAL_MS.
const MONITOR_INTERVAL: Duration = Duration::from_millis(400);
/// collectStream이 남기는 stderr 끝부분(UTF-16 단위).
const STDERR_LIMIT: usize = 200_000;
/// terminateProcessGroup의 SIGKILL 유예.
const FORCE_KILL_AFTER: Duration = Duration::from_millis(1500);
/// 턴이 끝나 입력을 닫은 뒤 Codex app-server가 스스로 끝나기를 기다리는 시간.
const APP_SERVER_EXIT_GRACE: Duration = Duration::from_secs(10);
const STEER_UNSUPPORTED: &str = "이 직원의 현재 업무는 실행 중 끼어들기를 받을 수 없습니다. 다음 턴 예약으로 보내세요.";
/// BLOCKED_WORKSPACE_STATUSES.
const BLOCKED_WORKSPACE_STATUSES: [&str; 3] = ["awaiting_approval", "merging", "conflict"];
const SESSION_END_BUSY: &str = "이 직원의 실행 중인 업무가 끝난 뒤 CLI를 변경하세요.";

/// POST /api/agent-jobs가 넘기는 값. 본문 값은 Node처럼 원래 JSON 값 그대로 받는다.
#[derive(Debug, Clone, Default)]
pub struct StartRequest {
    pub character_id: String,
    pub prompt: Option<Value>,
    pub conversation_id: Option<Value>,
    /// 없으면(undefined) 빈 목록이고, null은 그대로 null이다.
    pub attachment_paths: Option<Value>,
    pub sender_character_id: Option<Value>,
    pub request_source: Option<String>,
    pub reply_recipient_id: Option<String>,
    /// 답변 자동 전달 서비스만 넣는다. 받는 턴이 이 전달을 확정한다.
    pub delivery_id: Option<String>,
}

/// prepareTurn 결과.
struct PreparedTurn {
    turn_id: String,
    reply_recipient_ids: Vec<String>,
    session_id: String,
    conversation_id: String,
    external_session_id: Option<String>,
    resume_execution_workdir: Option<String>,
    character: RuntimeCharacter,
}

/// 활성 세션 후보. activeSessionMatchesRuntime이 보는 열만 읽는다.
pub(super) struct ActiveSession {
    pub(super) id: String,
    pub(super) external_session_id: Option<String>,
    pub(super) conversation_id: String,
    pub(super) conversation_workdir: Option<String>,
    pub(super) session_repository_root: Option<String>,
    pub(super) workspace_repository_root: Option<String>,
    pub(super) resume_execution_workdir: Option<String>,
}

/// preparingJobs·preparingCharacters 등록. 준비가 어떻게 끝나든 풀린다.
struct Preparation<'a> {
    runtime: &'a Runtime,
    character_id: String,
}

impl Drop for Preparation<'_> {
    fn drop(&mut self) {
        let mut jobs = self.runtime.jobs();
        jobs.preparing_jobs = jobs.preparing_jobs.saturating_sub(1);
        jobs.preparing_characters.remove(&self.character_id);
    }
}

impl Runtime {
    /// 서비스를 한 번 붙인다. 이미 붙어 있으면 false다.
    pub fn attach_services(&self, services: RuntimeServices) -> bool {
        self.services.set(Arc::new(services)).is_ok()
    }

    /// 붙은 서비스. 없으면 잡 경로가 503이다.
    pub fn services(&self) -> Result<Arc<RuntimeServices>, AgentError> {
        self.services.get().cloned().ok_or(AgentError::Unavailable)
    }

    pub(crate) fn jobs(&self) -> MutexGuard<'_, JobTable> {
        self.jobs.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// 실행 중인 직원 ID. 시험과 상태 확인용이다.
    pub fn running_character_ids(&self) -> Vec<String> {
        let mut ids: Vec<String> = self.jobs().running.keys().cloned().collect();
        ids.sort();
        ids
    }

    pub(super) fn workdir_text(&self) -> Option<String> {
        self.workdir.as_deref().and_then(Path::to_str).map(str::to_string)
    }

    /// recoverInterruptedJobs. 재시작 전 실행 중이던 턴을 중단으로 닫고 진행 중 활동을 끝낸다.
    /// 작업 공간 관리자가 있으면 준비 중이던 공간을 지우고, 통합된 공간을 정리하고, 활성 공간이 온전한지 확인한다.
    pub async fn recover_interrupted_jobs(&self) -> Result<u64, AgentError> {
        let services = self.services()?;
        let interrupted: Vec<String> = sqlx::query_scalar(
            r#"WITH existing_terminal_turns AS (
                 SELECT id, status FROM turns WHERE status IN ('completed', 'failed', 'interrupted')
               ), interrupted_turns AS (
                 UPDATE turns
                 SET status = 'interrupted',
                     error_message = '백엔드가 재시작되어 이전 실시간 출력 연결이 종료됐습니다.',
                     ended_at = COALESCE(ended_at, now()),
                     updated_at = now()
                 WHERE status IN ('pending', 'running')
                 RETURNING id, task_workspace_id AS "taskWorkspaceID"
               ), terminal_turns AS (
                 SELECT id, status FROM existing_terminal_turns
                 UNION ALL
                 SELECT id, 'interrupted' AS status FROM interrupted_turns
               ), closed_activities AS (
                 UPDATE turn_activities AS activity
                 SET status = CASE WHEN turn.status = 'completed' THEN 'completed' ELSE 'failed' END,
                     collaboration = CASE
                       WHEN activity.kind = 'collaboration' AND jsonb_typeof(activity.collaboration) = 'object'
                       THEN jsonb_set(
                         activity.collaboration,
                         '{agentStatus}',
                         to_jsonb((CASE WHEN turn.status = 'completed' THEN 'completed' ELSE 'errored' END)::text),
                         true
                       )
                       ELSE activity.collaboration
                     END
                 FROM terminal_turns AS turn
                 WHERE activity.turn_id = turn.id AND activity.status = 'running'
                 RETURNING activity.id
               )
               SELECT id::text FROM interrupted_turns"#,
        )
        .fetch_all(&services.pool)
        .await?;
        let manager = services.workspace_manager.clone();
        let workspaces_with_status = async |status: &str| -> Result<Vec<_>, sqlx::Error> {
            let rows = sqlx::query(AssertSqlSafe(format!(
                "SELECT {} FROM task_workspaces AS workspace WHERE workspace.status = $1",
                workspace_columns("workspace")
            )))
            .bind(status)
            .fetch_all(&services.pool)
            .await?;
            rows.iter().map(workspace_from_row).collect()
        };
        let interrupted_provisioning = workspaces_with_status("provisioning").await?;
        let mut cleanup_failures = Vec::new();
        if let Some(manager) = &manager {
            for workspace in &interrupted_provisioning {
                if let Err(error) = manager.cleanup_provisioning(workspace).await {
                    cleanup_failures.push((workspace.id.clone(), error.message));
                }
            }
        }
        sqlx::query(
            "UPDATE task_workspaces
             SET status = 'failed',
                 error_message = CASE status
                   WHEN 'merging' THEN '백엔드 재시작 전에 통합이 시작됐습니다. 원본 Git 상태를 확인하세요.'
                   ELSE '백엔드 재시작 전에 작업 공간 준비가 끝나지 않았습니다.'
                 END,
                 updated_at = now()
             WHERE status IN ('provisioning', 'merging')",
        )
        .execute(&services.pool)
        .await?;
        for (workspace_id, message) in &cleanup_failures {
            sqlx::query("UPDATE task_workspaces SET error_message = $2, updated_at = now() WHERE id = $1::uuid AND status = 'failed'")
                .bind(workspace_id)
                .bind(format!("중단된 작업 공간 자동 정리에 실패했습니다. {message}"))
                .execute(&services.pool)
                .await?;
        }
        let Some(manager) = manager else {
            return Ok(interrupted.len() as u64);
        };
        for workspace in workspaces_with_status("merged").await? {
            if let Err(error) = manager.cleanup(&workspace).await {
                sqlx::query("UPDATE task_workspaces SET error_message = $2, updated_at = now() WHERE id = $1::uuid AND status = 'merged'")
                    .bind(&workspace.id)
                    .bind(format!("통합된 작업 공간 정리에 실패했습니다. {}", error.message))
                    .execute(&services.pool)
                    .await?;
            }
        }
        for workspace in workspaces_with_status("active").await? {
            if let Err(error) = manager.validate_workspace(&workspace).await {
                let message = format!("활성 작업 공간을 복구할 수 없습니다. {}", error.message);
                with_transaction(&services.pool, async |connection| {
                    sqlx::query(
                        "UPDATE task_workspaces SET status = 'failed', error_message = $2, updated_at = now()
                         WHERE id = $1::uuid AND status = 'active'",
                    )
                    .bind(&workspace.id)
                    .bind(&message)
                    .execute(&mut *connection)
                    .await
                })
                .await?;
            }
        }
        Ok(interrupted.len() as u64)
    }

    /// messageAvailability. 직원별 현재 접수 가능 여부다. 기록·세션의 active는 실행 상태가 아니다.
    /// 열린 터미널의 실행 중 턴·여는 중·닫는 중·전달 대기도 함께 본다.
    pub async fn message_availability(&self, character_id: Option<&str>) -> Result<Vec<Value>, AgentError> {
        let services = self.services()?;
        let rows = sqlx::query(
            "SELECT character.id AS character_id, character.name,
               EXISTS (SELECT 1 FROM active_cli_sessions active
                 JOIN cli_sessions session ON session.id = active.cli_session_id
                 WHERE active.character_id = character.id AND session.ended_at IS NULL) AS session_open,
               busy.id::text AS turn_id, busy.status AS turn_status
             FROM characters character
             LEFT JOIN LATERAL (
               SELECT turn.id, turn.status FROM turns turn
               JOIN cli_sessions session ON session.id = turn.cli_session_id
               WHERE session.character_id = character.id AND turn.status IN ('pending', 'running')
               ORDER BY turn.started_at DESC, turn.id DESC LIMIT 1
             ) busy ON true
             WHERE ($1::text IS NULL OR character.id = $1)
             ORDER BY character.id",
        )
        .bind(character_id)
        .fetch_all(&services.pool)
        .await?;
        let draining = self.is_draining();
        let terminals = self.terminal_sessions();
        let jobs = self.jobs();
        let mut employees = Vec::with_capacity(rows.len());
        for row in rows {
            let id: String = row.try_get("character_id")?;
            let turn_status: Option<String> = row.try_get("turn_status")?;
            let terminal = terminals.as_ref().map(|manager| manager.availability(&id)).unwrap_or_default();
            let state = if draining {
                "draining"
            } else if jobs.compacting.contains(&id) {
                "compacting"
            } else if jobs.running.contains_key(&id) || terminal.running || turn_status.as_deref() == Some("running") {
                "running"
            } else if jobs.preparing_characters.contains(&id) || terminal.opening {
                "preparing"
            } else if terminal.closing {
                "closing"
            } else if terminal.dispatch || turn_status.as_deref() == Some("pending") {
                "pending"
            } else {
                "ready"
            };
            employees.push(json!({
                "characterId": id,
                "name": row.try_get::<String, _>("name")?,
                "sessionOpen": row.try_get::<bool, _>("session_open")?,
                "turnId": row.try_get::<Option<String>, _>("turn_id")?,
                "turnStatus": turn_status,
                "state": state,
                "canReceive": state == "ready",
            }));
        }
        Ok(employees)
    }

    /// start. 응답을 기다리던 요청이 끊겨도 준비가 중간에 멈추지 않도록 별도 작업에서 돈다.
    pub async fn start(self: &Arc<Self>, request: StartRequest) -> Result<Value, AgentError> {
        let runtime = Arc::clone(self);
        tokio::spawn(async move { runtime.start_job(request).await })
            .await
            .unwrap_or_else(|join_error| Err(AgentError::Failed(join_error.to_string())))
    }

    async fn start_job(self: &Arc<Self>, request: StartRequest) -> Result<Value, AgentError> {
        if self.is_draining() {
            return Err(AgentError::Draining("백엔드가 안전한 전환을 준비 중이라 새 업무를 받지 않습니다.".into()));
        }
        // 열린(또는 여는 중인) 터미널이 있으면 그 PTY로만 전달한다. 대기열이 아니라 바쁘면 바로 거절한다.
        if let Some(terminals) = self.terminal_sessions()
            && terminals.has(&request.character_id)
        {
            return terminals.dispatch(request).await;
        }
        let services = self.services()?;
        let character_id = request.character_id.clone();
        {
            let mut jobs = self.jobs();
            if jobs.compacting.contains(&character_id) || jobs.preparing_characters.contains(&character_id) {
                return Err(AgentError::Busy("이 직원의 컨텍스트 압축 또는 업무 준비가 끝난 뒤 시작하세요.".into()));
            }
            jobs.preparing_jobs += 1;
            jobs.preparing_characters.insert(character_id.clone());
        }
        let _preparation = Preparation { runtime: self, character_id };
        self.start_accepted(&services, request).await
    }

    /// startAccepted.
    async fn start_accepted(self: &Arc<Self>, services: &Arc<RuntimeServices>, request: StartRequest) -> Result<Value, AgentError> {
        let character_id = request.character_id.as_str();
        let prompt_text = match &request.prompt {
            None | Some(Value::Null) => String::new(),
            Some(prompt) => js_string(prompt),
        };
        let clean_prompt = js_trim(&prompt_text).to_string();
        let attachment_paths = request.attachment_paths.clone().unwrap_or_else(|| json!([]));
        if clean_prompt.is_empty() && attachment_list_is_empty(&attachment_paths)? {
            return Err(AgentError::Failed("업무 내용을 입력하세요.".into()));
        }
        {
            let jobs = self.jobs();
            if jobs.running.contains_key(character_id) || jobs.compacting.contains(character_id) {
                return Err(AgentError::Busy("이 직원의 현재 업무가 끝난 뒤 새 업무를 시작하세요.".into()));
            }
        }
        let Some(workdir) = self.workdir_text() else {
            return Err(AgentError::Unavailable);
        };
        let record_prompt = if clean_prompt.is_empty() { "첨부 파일을 확인해줘.".to_string() } else { clean_prompt };
        let conversation_id = match request.conversation_id.as_ref() {
            Some(value) if js_truthy(Some(value)) => pg_text(value).unwrap_or_default(),
            _ => random_uuid(),
        };
        let prepared = self
            .prepare_turn(
                services,
                character_id,
                &record_prompt,
                &conversation_id,
                request.sender_character_id.as_ref(),
                request.request_source.as_deref(),
                request.delivery_id.as_deref(),
            )
            .await?;

        let mut attachments = Vec::new();
        let mut structured_result_path: Option<String> = None;
        let staged = async {
            attachments = stage_attachments(&attachment_paths, &workdir)?;
            let effective_prompt = prompt_with_attachments(&record_prompt, &attachments);
            let execution_prompt = reply_routing_prompt(&effective_prompt, character_id, &prepared.reply_recipient_ids);
            structured_result_path = Some(prepare_structured_turn_result(
                &workdir,
                prepared.character.id.as_deref(),
                &services.paths.runtime_id,
            )?);
            self.begin_prepared_turn(services, &prepared.turn_id, &effective_prompt).await?;
            Ok::<_, String>((effective_prompt, execution_prompt))
        }
        .await;
        let (effective_prompt, execution_prompt) = match staged {
            Ok(prompts) => prompts,
            Err(message) => {
                discard_structured_turn_result(structured_result_path.as_deref());
                remove_staged_attachments(&attachments);
                // 준비 단계의 원래 실패 원인을 호출자에게 유지한다.
                let _ = self.fail_prepared_turn(services, &prepared, &message).await;
                return Err(AgentError::Failed(message));
            }
        };

        let backend = prepared.character.backend.clone();
        let external = prepared.external_session_id.clone().filter(|id| !id.is_empty());
        let resumed_codex_session = backend == "codex" && external.is_some();
        let resumed_antigravity_session = backend == "antigravity" && external.is_some();
        let roots = &services.roots;
        let usage_baseline = match external.as_deref() {
            Some(id) if resumed_codex_session => {
                latest_codex_usage_from_rollout(find_rollout_path(&roots.codex_sessions(), id).as_deref())
            }
            Some(id) if resumed_antigravity_session => antigravity_session_usage(id, &roots.antigravity_conversations()),
            _ => None,
        };
        let antigravity_reasoning_baseline = match external.as_deref() {
            Some(id) if resumed_antigravity_session => antigravity_latest_step_index(id, &roots.antigravity_conversations()),
            _ => None,
        };
        let usage_rollout_offset = match external.as_deref() {
            Some(id) if resumed_codex_session => codex_rollout_size(&roots.codex_sessions(), id),
            _ => 0,
        };
        let session_key = external.as_deref().unwrap_or("");
        let image_root = roots.generated_images(&backend);
        let initial_generated_images = list_generated_images(session_key, &image_root)
            .map_err(|error| AgentError::Failed(scandir_error(&error, &image_root, session_key)))?;
        let rollout_reader = match external.as_deref() {
            Some(id) if backend == "codex" => RolloutReader::open(&roots.codex_sessions(), id, true),
            _ => None,
        };

        let turn_id = prepared.turn_id.clone();
        let conversation_id = prepared.conversation_id.clone();
        let mut state = TurnState::new(TurnSeed {
            turn_id: prepared.turn_id,
            session_id: prepared.session_id,
            conversation_id: prepared.conversation_id,
            external_session_id: prepared.external_session_id,
            resume_execution_workdir: prepared.resume_execution_workdir,
            character: prepared.character,
            prompt: effective_prompt,
            execution_prompt,
            workdir,
            structured_result_path,
            attachments,
            reply_recipient_ids: prepared.reply_recipient_ids,
        });
        state.usage_rollout_offset = usage_rollout_offset;
        state.initial_generated_images = initial_generated_images.into_iter().collect();
        state.rollout_reader = rollout_reader;
        state.resumed_codex_session = resumed_codex_session;
        state.resumed_antigravity_session = resumed_antigravity_session;
        state.antigravity_reasoning_baseline = antigravity_reasoning_baseline;
        state.usage_baseline = usage_baseline;
        let handle = TurnHandle::new(state);
        self.jobs().running.insert(character_id.to_string(), Arc::clone(&handle));
        services.broadcast(json!({ "type": "feed.changed", "turnId": turn_id }));

        let runtime = Arc::clone(self);
        let job_services = Arc::clone(services);
        tokio::spawn(async move {
            if let Err(error) = runtime.execute(&job_services, &handle).await {
                runtime.fail(&job_services, &handle, &error).await;
            }
        });
        Ok(json!({ "turnId": turn_id, "conversationId": conversation_id, "status": "running" }))
    }

    /// prepareTurn. 직원 잠금 아래에서 바쁜지 확인하고, 맞는 활성 세션을 이어 쓰거나 새 세션을 열어 대기 턴을 만든다.
    /// 로컬 모델 직원은 정의를 붙이고, 이어 쓸 세션이 같은 로컬 정의로 시작됐는지 확인한 뒤 턴에 정의를 남긴다.
    #[allow(clippy::too_many_arguments)]
    async fn prepare_turn(
        &self,
        services: &RuntimeServices,
        character_id: &str,
        prompt: &str,
        conversation_id: &str,
        sender_character_id: Option<&Value>,
        request_source: Option<&str>,
        delivery_id: Option<&str>,
    ) -> Result<PreparedTurn, AgentError> {
        let workdir = self.workdir_text();
        let repository_root = self.repository_root.as_deref().and_then(Path::to_str).map(str::to_string);
        with_transaction(&services.pool, async |connection| {
            sqlx::query("SELECT pg_advisory_xact_lock(hashtext($1))")
                .bind(format!("officestra:character:{character_id}"))
                .execute(&mut *connection)
                .await?;
            let Some(row) = sqlx::query(
                "SELECT id, name, seat, backend, model, effort, fast_mode, auto_compact_percent, permission,
                   identity_prompt, config
                 FROM characters WHERE id = $1 FOR UPDATE",
            )
            .bind(character_id)
            .fetch_optional(&mut *connection)
            .await?
            else {
                return Err(AgentError::CharacterNotFound("캐릭터를 찾을 수 없습니다.".into()));
            };
            let mut character = RuntimeCharacter {
                id: Some(row.try_get("id")?),
                name: row.try_get("name")?,
                seat: row.try_get("seat")?,
                backend: row.try_get("backend")?,
                model: row.try_get("model")?,
                effort: row.try_get("effort")?,
                fast_mode: row.try_get("fast_mode")?,
                auto_compact_percent: Some(f64::from(row.try_get::<i16, _>("auto_compact_percent")?)),
                permission: row.try_get("permission")?,
                identity_prompt: row.try_get("identity_prompt")?,
                config: row.try_get("config")?,
                local_profile: None,
            };

            let busy: Option<String> = sqlx::query_scalar(
                "SELECT turn.id::text FROM turns AS turn
                 JOIN cli_sessions AS session ON session.id = turn.cli_session_id
                 WHERE session.character_id = $1 AND turn.status IN ('pending', 'running')
                 LIMIT 1",
            )
            .bind(character_id)
            .fetch_optional(&mut *connection)
            .await?;
            if busy.is_some() {
                return Err(AgentError::Busy("이 직원은 이미 업무를 처리하고 있습니다.".into()));
            }

            let candidate = active_session(connection, character_id).await?;
            let active = candidate
                .filter(|row| active_session_matches_runtime(row, workdir.as_deref(), repository_root.as_deref()));
            let (session_id, external_session_id, effective_conversation_id, resume_execution_workdir) = match active {
                Some(active) => {
                    (active.id, active.external_session_id, active.conversation_id, active.resume_execution_workdir)
                }
                None => {
                    sqlx::query(
                        "INSERT INTO conversations (id, title, workdir) VALUES ($1::uuid, $2, $3) ON CONFLICT (id) DO NOTHING",
                    )
                    .bind(conversation_id)
                    .bind(utf16_prefix(prompt, 60))
                    .bind(workdir.as_deref())
                    .execute(&mut *connection)
                    .await?;
                    let previous: Option<String> = sqlx::query_scalar(
                        "SELECT id::text FROM cli_sessions WHERE character_id = $1 ORDER BY started_at DESC, id DESC LIMIT 1",
                    )
                    .bind(character_id)
                    .fetch_optional(&mut *connection)
                    .await?;
                    let session_id: String = sqlx::query_scalar(
                        "INSERT INTO cli_sessions (conversation_id, character_id, previous_session_id)
                         VALUES ($1::uuid, $2, $3::uuid) RETURNING id::text",
                    )
                    .bind(conversation_id)
                    .bind(character_id)
                    .bind(previous)
                    .fetch_one(&mut *connection)
                    .await?;
                    (session_id, None, conversation_id.to_string(), None)
                }
            };

            let definition = resolve_local_character(
                connection,
                &character.config,
                &character.backend,
                character.model.as_deref(),
                &character.effort,
                character.fast_mode,
            )
            .await?;
            character.local_profile = definition.map(|definition| definition.to_value());
            validate_local_resume(connection, &session_id, character.local_profile.as_ref(), external_session_id.as_deref()).await?;
            let turn_id = random_uuid();
            sqlx::query(
                "INSERT INTO turns (id, cli_session_id, task_workspace_id, backend, model, effort, fast_mode, prompt,
                   sender_character_id, request_source, status, started_at, updated_at)
                 VALUES ($1::uuid, $2::uuid, NULL, $3, $4, $5, $6, $7, $8, $9, 'pending', now(), now())",
            )
            .bind(&turn_id)
            .bind(&session_id)
            .bind(&character.backend)
            .bind(&character.model)
            .bind(&character.effort)
            .bind(character.fast_mode)
            .bind(prompt)
            .bind(sender_character_id.and_then(pg_text))
            .bind(request_source)
            .execute(&mut *connection)
            .await?;
            sqlx::query("INSERT INTO messages (turn_id, role, text) VALUES ($1::uuid, 'user', $2), ($1::uuid, 'assistant', '')")
                .bind(&turn_id)
                .bind(prompt)
                .execute(&mut *connection)
                .await?;
            snapshot_local_turn(connection, &turn_id, character.local_profile.as_ref()).await?;
            let reply_recipient_ids = record_reply_delivery(connection, &turn_id, character_id, delivery_id)
                .await
                .map_err(|error| match error {
                    ReplyDeliveryError::Database(error) => AgentError::from(error),
                    other => AgentError::Failed(other.to_string()),
                })?;
            Ok(PreparedTurn {
                turn_id,
                reply_recipient_ids,
                session_id,
                conversation_id: effective_conversation_id,
                external_session_id,
                resume_execution_workdir,
                character,
            })
        })
        .await
    }

    /// beginPreparedTurn. 첨부를 붙인 프롬프트로 바꾸고 실행 중으로 올린다.
    async fn begin_prepared_turn(&self, services: &RuntimeServices, turn_id: &str, prompt: &str) -> Result<(), String> {
        with_transaction(&services.pool, async |connection| {
            sqlx::query(
                "UPDATE turns SET prompt = $2, status = 'running', started_at = now(), updated_at = now()
                 WHERE id = $1::uuid AND status = 'pending'",
            )
            .bind(turn_id)
            .bind(prompt)
            .execute(&mut *connection)
            .await?;
            sqlx::query("UPDATE messages SET text = $2, received_at = now() WHERE turn_id = $1::uuid AND role = 'user'")
                .bind(turn_id)
                .bind(prompt)
                .execute(&mut *connection)
                .await?;
            Ok::<_, AgentError>(())
        })
        .await
        .map_err(|error| error.to_string())
    }

    /// failPreparedTurn. 실행 전에 실패한 대기 턴을 닫고, 새로 연 세션이면 함께 끝낸다.
    async fn fail_prepared_turn(&self, services: &RuntimeServices, prepared: &PreparedTurn, message: &str) -> Result<(), AgentError> {
        with_transaction(&services.pool, async |connection| {
            sqlx::query(
                "UPDATE turns SET status = 'failed', error_message = $2, ended_at = now(), updated_at = now()
                 WHERE id = $1::uuid AND status = 'pending'",
            )
            .bind(&prepared.turn_id)
            .bind(message)
            .execute(&mut *connection)
            .await?;
            if prepared.external_session_id.as_deref().is_none_or(str::is_empty) {
                sqlx::query("UPDATE cli_sessions SET ended_at = COALESCE(ended_at, now()) WHERE id = $1::uuid")
                    .bind(&prepared.session_id)
                    .execute(&mut *connection)
                    .await?;
            }
            Ok::<_, AgentError>(())
        })
        .await?;
        services.broadcast(json!({
            "type": "feed.changed",
            "turnId": prepared.turn_id,
            "characterId": prepared.character.id,
        }));
        Ok(())
    }
}

/// prepareTurn의 활성 세션 조회. 작업 공간 열은 활성 세션 비교에 쓰는 저장소 루트만 읽는다.
async fn active_session(connection: &mut PgConnection, character_id: &str) -> Result<Option<ActiveSession>, AgentError> {
    let row = sqlx::query(
        "SELECT session.id::text AS id, session.external_id, session.conversation_id::text AS conversation_id,
           conversation.workdir AS conversation_workdir,
           session_scope.repository_root AS session_repository_root,
           workspace.repository_root AS workspace_repository_root,
           resume_workspace.execution_workdir AS resume_execution_workdir
         FROM active_cli_sessions AS active
         JOIN cli_sessions AS session ON session.id = active.cli_session_id
         JOIN conversations AS conversation ON conversation.id = session.conversation_id
         LEFT JOIN LATERAL (
           SELECT candidate.repository_root FROM task_workspaces AS candidate
           WHERE candidate.cli_session_id = session.id AND candidate.repository_root IS NOT NULL
           ORDER BY candidate.updated_at DESC, candidate.id DESC LIMIT 1
         ) AS session_scope ON true
         LEFT JOIN LATERAL (
           SELECT candidate.repository_root FROM task_workspaces AS candidate
           WHERE candidate.cli_session_id = session.id AND candidate.status IN ('provisioning', 'active')
           ORDER BY candidate.updated_at DESC LIMIT 1
         ) AS workspace ON true
         LEFT JOIN LATERAL (
           SELECT candidate.execution_workdir FROM turns AS completed_turn
           JOIN task_workspaces AS candidate ON candidate.id = completed_turn.task_workspace_id
           WHERE completed_turn.cli_session_id = session.id AND completed_turn.status = 'completed'
             AND candidate.execution_workdir IS NOT NULL
           ORDER BY completed_turn.ended_at DESC NULLS LAST, completed_turn.started_at DESC, completed_turn.id DESC
           LIMIT 1
         ) AS resume_workspace ON true
         WHERE active.character_id = $1 AND session.ended_at IS NULL
         LIMIT 1",
    )
    .bind(character_id)
    .fetch_optional(&mut *connection)
    .await?;
    let Some(row) = row else {
        return Ok(None);
    };
    Ok(Some(ActiveSession {
        id: row.try_get("id")?,
        external_session_id: row.try_get("external_id")?,
        conversation_id: row.try_get("conversation_id")?,
        conversation_workdir: row.try_get("conversation_workdir")?,
        session_repository_root: row.try_get("session_repository_root")?,
        workspace_repository_root: row.try_get("workspace_repository_root")?,
        resume_execution_workdir: row.try_get("resume_execution_workdir")?,
    }))
}

/// activeSessionMatchesRuntime. 세션의 저장소 루트가 있으면 그것으로, 없으면 대화 폴더로 현재 실행기와 비교한다.
pub(super) fn active_session_matches_runtime(row: &ActiveSession, workdir: Option<&str>, repository_root: Option<&str>) -> bool {
    let current_workdir = canonical_runtime_path(workdir);
    let current_repository_root = canonical_runtime_path(repository_root);
    let session_root = row.session_repository_root.as_deref().or(row.workspace_repository_root.as_deref());
    if let Some(session_root) = canonical_runtime_path(session_root) {
        return Some(session_root) == current_repository_root;
    }
    let Some(conversation_workdir) = canonical_runtime_path(row.conversation_workdir.as_deref()) else {
        return false;
    };
    Some(&conversation_workdir) == current_workdir.as_ref() || Some(&conversation_workdir) == current_repository_root.as_ref()
}

/// canonicalRuntimePath.
fn canonical_runtime_path(value: Option<&str>) -> Option<String> {
    let candidate = js_trim(value.unwrap_or(""));
    if candidate.is_empty() {
        return None;
    }
    let absolute = resolve_path(candidate);
    Some(realpath_sync(&absolute).unwrap_or(absolute))
}

/// `attachmentPaths.length === 0`. null이면 Node처럼 속성을 읽지 못한 오류다.
fn attachment_list_is_empty(paths: &Value) -> Result<bool, AgentError> {
    match paths {
        Value::Null => Err(AgentError::Failed("Cannot read properties of null (reading 'length')".into())),
        Value::Array(items) => Ok(items.is_empty()),
        Value::String(text) => Ok(text.is_empty()),
        Value::Object(fields) => Ok(fields.get("length").and_then(Value::as_f64) == Some(0.0)),
        _ => Ok(false),
    }
}

/// readdirSync 오류 문구.
fn scandir_error(error: &std::io::Error, root: &str, session_id: &str) -> String {
    let (code, description) = uv_error(error);
    format!("{code}: {description}, scandir '{}'", join_path(&[root, session_id]))
}

// ---- 실행 ----

impl Runtime {
    /// execute. 로컬 모델 직원은 4090 앞문을 연 CLI 한 번, Claude는 지속 세션, 나머지는 턴마다 띄우는 CLI다.
    async fn execute(self: &Arc<Self>, services: &Arc<RuntimeServices>, handle: &Arc<TurnHandle>) -> Result<(), String> {
        let (backend, local) = {
            let state = handle.state.lock().await;
            (state.backend().to_string(), state.character.local_profile.is_some())
        };
        if local {
            self.execute_local(services, handle).await
        } else if backend == "claude" {
            self.execute_claude(services, handle).await
        } else {
            self.execute_single_process(services, handle, None).await
        }
    }

    /// execute의 로컬 분기. 앞문은 실행이 어떻게 끝나든 돌려준다(Node finally). 돌려주기가 실패하면 그 오류가 이긴다.
    async fn execute_local(self: &Arc<Self>, services: &Arc<RuntimeServices>, handle: &Arc<TurnHandle>) -> Result<(), String> {
        let Some(providers) = services.local_providers.clone() else {
            return Err("Local provider service unavailable".into());
        };
        // 앞문을 여는 동안 턴 상태를 잠가 두지 않도록 필요한 값만 꺼낸다.
        let (character, prompt, previous_session_id, resume_workdir, attachments, workdir) = {
            let state = handle.state.lock().await;
            (
                state.character.clone(),
                state.execution_prompt.clone(),
                state.external_session_id.clone().filter(|id| !id.is_empty()),
                state.resume_execution_workdir.clone(),
                state.attachments.clone(),
                state.workdir.clone(),
            )
        };
        if character.backend == "claude"
            && let Some(external) = previous_session_id.as_deref()
        {
            prepare_claude_session_resume(&services.roots.claude_projects(), external, Some(&workdir), resume_workdir.as_deref())?;
        }
        let definition = normalize_local_definition(character.local_profile.as_ref().unwrap_or(&Value::Null))?;
        let executable = locate_executable(&character.backend, &character.config)?;
        let launch = providers
            .launch(
                &character,
                &definition,
                LaunchRequest {
                    mode: "gui",
                    prompt: &prompt,
                    attachments: &attachments,
                    previous_session_id: previous_session_id.as_deref(),
                    workdir: &workdir,
                    base_environment: None,
                    // GUI 실행 인자는 터미널 훅을 쓰지 않는다.
                    hook_path: "",
                    executable: &executable,
                    paths: &services.paths,
                },
            )
            .await
            .map_err(|error| error.message)?;
        let result = self.execute_single_process(services, handle, Some(&launch.launch)).await;
        launch.release.release().await.map_err(|error| error.message)?;
        result
    }

    /// executeSingleProcess. stdout은 줄마다 처리하고 stderr는 끝부분만 모은다. 로컬 명세가 있으면 그 실행 파일·인자·환경을 쓴다.
    async fn execute_single_process(
        self: &Arc<Self>,
        services: &Arc<RuntimeServices>,
        handle: &Arc<TurnHandle>,
        local: Option<&LocalAgentLaunch>,
    ) -> Result<(), String> {
        let (executable, arguments, environment, workdir, backend, app_server) = {
            let state = handle.state.lock().await;
            // 클라우드 Codex는 실행 중 끼어들기를 받으려고 exec 대신 app-server로 한 턴을 돌린다.
            let app_server = (local.is_none() && state.backend() == "codex").then(|| {
                let images: Vec<String> =
                    state.attachments.iter().filter(|attachment| attachment.is_codex_image).map(|attachment| attachment.path.clone()).collect();
                CodexTurnRequest {
                    thread_id: state.external_session_id.clone().filter(|id| !id.is_empty()),
                    cwd: state.workdir.clone(),
                    sandbox: state.character.permission.clone(),
                    input: codex_turn_input(&state.execution_prompt, &images),
                }
            });
            let (executable, arguments, environment) = match local {
                Some(local) => (local.executable.clone(), local.args.clone(), local.env.clone()),
                None => (
                    locate_executable(state.backend(), &state.character.config)?,
                    if app_server.is_some() {
                        codex_app_server_arguments(&state.character)
                    } else {
                        build_arguments(
                            &state.character,
                            &state.execution_prompt,
                            state.external_session_id.as_deref(),
                            &state.attachments,
                            Some(&state.workdir),
                        )?
                    },
                    execution_environment(&state.character, &process_environment(), Some(&state.workdir), &services.paths),
                ),
            };
            (executable, arguments, environment, state.workdir.clone(), state.backend().to_string(), app_server)
        };
        let mut command = detached_command(&executable, &arguments, Some(&workdir), &environment);
        let input = if app_server.is_some() { Stdio::piped() } else { Stdio::null() };
        command.stdin(input).stdout(Stdio::piped()).stderr(Stdio::piped());
        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(error) => {
                // Node는 spawn 오류가 close 대기에서 터져도 finally에서 감시를 멈추며 마지막으로 한 번 더 읽는다.
                final_monitor_poll(services, handle, &backend).await;
                return Err(format!("spawn {executable} {}", uv_error(&error).0));
            }
        };
        let process = ProcessGroup::new(child.id());
        handle.set_process(Some(process.clone()));
        if handle.cancel_requested() {
            // 프로세스를 넣기 전에 들어온 중단은 여기서 끝낸다.
            process.terminate(FORCE_KILL_AFTER, false);
        }
        let monitor = OutputMonitor::start(services, handle, &backend);
        let stderr = child.stderr.take();
        let error_task = tokio::spawn(async move {
            match stderr {
                Some(stderr) => collect_stream(stderr).await,
                None => String::new(),
            }
        });
        let mut app_server_ended_early = false;
        let uses_app_server = app_server.is_some();
        let output = match (child.stdout.take(), app_server) {
            (Some(stdout), Some(request)) => {
                let (sender, receiver) = mpsc::unbounded_channel();
                handle.set_codex_steer(Some(sender));
                let (line_handle, line_services) = (Arc::clone(handle), Arc::clone(services));
                let on_line = move |line: String| {
                    let (handle, services) = (Arc::clone(&line_handle), Arc::clone(&line_services));
                    async move { handle.state.lock().await.consume_output_line(&services, &line).await }
                };
                let result = codex_app_server::drive(child.stdin.take(), stdout, request, receiver, on_line).await;
                handle.set_codex_steer(None);
                match result {
                    Ok(completed) => {
                        app_server_ended_early = !completed;
                        handle.state.lock().await.complete_pending_initial_codex_reasoning(services, None).await
                    }
                    Err(error) => Err(error),
                }
            }
            (Some(stdout), None) => consume_output(services, handle, stdout).await,
            (None, _) => Ok(()),
        };
        let status = if uses_app_server {
            match tokio::time::timeout(APP_SERVER_EXIT_GRACE, child.wait()).await {
                Ok(status) => status,
                Err(_) => {
                    // app-server는 입력을 닫으면 끝난다. 그래도 남아 있으면 프로세스 그룹을 끝낸다.
                    process.terminate(FORCE_KILL_AFTER, false);
                    child.wait().await
                }
            }
        } else {
            child.wait().await
        };
        if let Ok(status) = &status {
            process.record_exit(status);
        }
        monitor.stop(services, handle, &backend).await;
        let status = status.map_err(|error| error.to_string())?;
        output?;
        let stderr = error_task.await.unwrap_or_default();
        let stderr = js_trim(&stderr).to_string();
        let exit_code = status.code();
        let exit_text = exit_code.map_or_else(|| "null".to_string(), |code| code.to_string());

        let mut state = handle.state.lock().await;
        if state.settle_cancelled_output(services).await? {
            return Ok(());
        }
        if app_server_ended_early {
            if !stderr.is_empty() {
                return Err(stderr);
            }
            return Err(format!("Codex app-server가 업무를 끝내기 전에 종료 코드 {exit_text}로 끝났습니다."));
        }
        if let Some(failure) = state.failure.clone().filter(|failure| !failure.is_empty()) {
            // agy는 한도 오류가 한 번 난 대화를 이어 가면 정상 종료한 턴에도 그때의 오류를 보낸다. 답이 있으면 경고로 낮춘다.
            let stale_antigravity_failure = state.backend() == "antigravity"
                && exit_code == Some(0)
                && !js_trim(&state.final_response_candidate()).is_empty();
            if !stale_antigravity_failure {
                return Err(failure);
            }
            eprintln!(
                "[agent-runtime] {} Antigravity가 정상 종료했지만 이전 오류를 보고해 경고로 처리합니다. ({failure})",
                state.character_id()
            );
        }
        let candidate = state.final_response_candidate();
        let decoded = state.decode_completed_response(&candidate);
        if !js_truthy(decoded.get("text")) {
            if exit_code != Some(0) {
                if !stderr.is_empty() {
                    return Err(stderr);
                }
                if let Some(warning) = state.warning.clone().filter(|warning| !warning.is_empty()) {
                    return Err(warning);
                }
                return Err(format!("CLI가 종료 코드 {exit_text}로 끝났습니다."));
            }
            return Err("CLI 최종 메시지가 없습니다.".into());
        }
        if exit_code != Some(0) {
            eprintln!(
                "[agent-runtime] {} CLI가 종료 코드 {exit_text}로 끝났지만 응답 텍스트가 정상 확보되어 완료 처리합니다. (경고: {}, stderr: {})",
                state.character_id(),
                state.warning.as_deref().unwrap_or("없음"),
                if stderr.is_empty() { "없음" } else { &stderr },
            );
        }
        self.complete(services, handle, &mut state, &decoded).await?;
        let (character, session_id) = (state.character.clone(), state.external_session_id.clone());
        drop(state);
        self.maybe_auto_compact_after_turn(services, &character, session_id.as_deref()).await;
        Ok(())
    }

    /// executeClaude. 직원별 지속 세션에 메시지를 보내고 result까지 받는다.
    async fn execute_claude(self: &Arc<Self>, services: &Arc<RuntimeServices>, handle: &Arc<TurnHandle>) -> Result<(), String> {
        let (worker, prompt) = {
            let state = handle.state.lock().await;
            if let Some(session_id) = state.external_session_id.as_deref().filter(|id| !id.is_empty()) {
                prepare_claude_session_resume(
                    &services.roots.claude_projects(),
                    session_id,
                    Some(&state.workdir),
                    state.resume_execution_workdir.as_deref(),
                )?;
            }
            let executable = locate_executable("claude", &state.character.config)?;
            let worker = self.acquire_claude_worker(
                services,
                &state.character,
                &state.workdir,
                state.external_session_id.as_deref(),
                &executable,
            );
            (worker, state.execution_prompt.clone())
        };
        handle.set_claude_worker(Some(Arc::clone(&worker)));
        let result = if handle.cancel_requested() {
            // 지속 세션을 넣기 전에 들어온 중단이다. 이 턴은 보내지 않는다.
            Err("사용자가 업무를 중단했습니다.".to_string())
        } else {
            let line_handle = Arc::clone(handle);
            let line_services = Arc::clone(services);
            worker
                .run_turn(&prompt, move |line| {
                    let handle = Arc::clone(&line_handle);
                    let services = Arc::clone(&line_services);
                    async move { handle.state.lock().await.consume_output_line(&services, &line).await }
                })
                .await
        };
        let mut state = handle.state.lock().await;
        if let Err(error) = result {
            if state.settle_cancelled_output(services).await? {
                return Ok(());
            }
            return Err(error);
        }
        if state.settle_cancelled_output(services).await? {
            return Ok(());
        }
        if let Some(failure) = state.failure.clone().filter(|failure| !failure.is_empty()) {
            return Err(failure);
        }
        let candidate = state.final_response_candidate();
        let decoded = state.decode_completed_response(&candidate);
        if !js_truthy(decoded.get("text")) {
            return Err("CLI 최종 메시지가 없습니다.".into());
        }
        self.complete(services, handle, &mut state, &decoded).await?;
        let (character, session_id) = (state.character.clone(), state.external_session_id.clone());
        drop(state);
        self.maybe_auto_compact_after_turn(services, &character, session_id.as_deref()).await;
        Ok(())
    }

    /// acquireClaudeWorker. 설정·작업 폴더·세션이 같으면 기존 지속 세션을 쓰고, 다르면 닫고 새로 띄운다.
    /// Node처럼 업무 턴과 컨텍스트 압축이 같이 쓴다.
    pub(super) fn acquire_claude_worker(
        self: &Arc<Self>,
        services: &RuntimeServices,
        character: &RuntimeCharacter,
        workdir: &str,
        session_id: Option<&str>,
        executable: &str,
    ) -> Arc<ClaudePersistentWorker> {
        let character_id = character.id.clone().unwrap_or_default();
        let signature = claude_persistent_worker_signature(character, executable, workdir);
        let existing = self.jobs().claude_workers.get(&character_id).cloned();
        if let Some(existing) = existing {
            if existing.matches(&signature, session_id) {
                return existing;
            }
            // Node처럼 표에서 지우지는 않는다. 닫힌 세션의 종료 알림이 같은 세션일 때만 지운다.
            existing.close("Claude Code 설정 또는 작업 공간이 바뀌어 지속 세션을 교체합니다.");
        }
        let runtime = Arc::downgrade(self);
        let exit_character_id = character_id.clone();
        let projects = services.roots.claude_projects();
        let worker = ClaudePersistentWorker::spawn(WorkerOptions {
            executable: executable.to_string(),
            arguments: claude_persistent_arguments(character, session_id),
            cwd: Some(workdir.to_string()),
            env: execution_environment(character, &process_environment(), Some(workdir), &services.paths),
            signature,
            session_id: session_id.map(str::to_string),
            resume_transcript_path: session_id.and_then(|id| claude_session_path(&projects, Some(workdir), id)),
            on_exit: Box::new(move |worker_id| {
                let Some(runtime) = runtime.upgrade() else {
                    return;
                };
                let mut jobs = runtime.jobs();
                if jobs.claude_workers.get(&exit_character_id).is_some_and(|worker| worker.id == worker_id) {
                    jobs.claude_workers.remove(&exit_character_id);
                }
            }),
            suggestion_grace: services.claude_suggestion_grace,
            interrupt_result_timeout: services.claude_interrupt_result_timeout,
            steer_result_timeout: DEFAULT_STEER_RESULT_TIMEOUT,
        });
        self.jobs().claude_workers.insert(character_id, Arc::clone(&worker));
        worker
    }

    /// closeClaudeWorker. expected가 있으면 그 세션일 때만 닫는다. 닫기는 잡 표 잠금 밖에서 한다.
    pub fn close_claude_worker(&self, character_id: &str, expected: Option<&Arc<ClaudePersistentWorker>>, reason: &str) -> bool {
        let worker = {
            let mut jobs = self.jobs();
            let Some(worker) = jobs.claude_workers.get(character_id) else {
                return false;
            };
            if expected.is_some_and(|expected| !Arc::ptr_eq(expected, worker)) {
                return false;
            }
            jobs.claude_workers.remove(character_id)
        };
        if let Some(worker) = worker {
            worker.close(reason);
        }
        true
    }

    /// closeClaudeWorkers.
    pub fn close_claude_workers(&self, reason: &str) {
        let workers: Vec<_> = self.jobs().claude_workers.drain().map(|(_, worker)| worker).collect();
        for worker in workers {
            worker.close(reason);
        }
    }

    /// shutdown. 임베딩 서비스 닫기는 서버가 따로 한다.
    pub fn shutdown(&self) {
        self.close_claude_workers("OFFICESTRA 백엔드가 종료됩니다.");
    }
}

/// Codex rollout·Antigravity 대화 기록 감시. 400ms마다 상태 잠금을 잡고 한 번씩 읽는다.
struct OutputMonitor {
    running: Option<(oneshot::Sender<()>, tokio::task::JoinHandle<()>)>,
}

impl OutputMonitor {
    fn start(services: &Arc<RuntimeServices>, handle: &Arc<TurnHandle>, backend: &str) -> Self {
        let codex = match backend {
            "codex" => true,
            "antigravity" => false,
            _ => return Self { running: None },
        };
        let (stop, mut stopped) = oneshot::channel::<()>();
        let services = Arc::clone(services);
        let handle = Arc::clone(handle);
        let task = tokio::spawn(async move {
            let mut interval = tokio::time::interval_at(tokio::time::Instant::now() + MONITOR_INTERVAL, MONITOR_INTERVAL);
            interval.set_missed_tick_behavior(MissedTickBehavior::Skip);
            loop {
                tokio::select! {
                    _ = &mut stopped => break,
                    _ = interval.tick() => {
                        let mut state = handle.state.lock().await;
                        if codex {
                            if let Err(error) = state.consume_codex_rollout_activities(&services).await {
                                eprintln!("Codex 협업 기록을 읽지 못했습니다. {error}");
                            }
                        } else if let Err(error) = state.consume_antigravity_reasoning(&services, false).await {
                            eprintln!("Antigravity 추론 기록을 읽지 못했습니다. {error}");
                        }
                    }
                }
            }
        });
        Self { running: Some((stop, task)) }
    }

    /// stopCodexRolloutMonitor·stopAntigravityReasoningMonitor. 진행 중인 읽기를 기다린 뒤 마지막으로 한 번 더 읽는다.
    async fn stop(self, services: &RuntimeServices, handle: &TurnHandle, backend: &str) {
        if let Some((stop, task)) = self.running {
            let _ = stop.send(());
            let _ = task.await;
        }
        final_monitor_poll(services, handle, backend).await;
    }
}

async fn final_monitor_poll(services: &RuntimeServices, handle: &TurnHandle, backend: &str) {
    match backend {
        "codex" => {
            for attempt in 0..2 {
                let result = handle.state.lock().await.consume_codex_rollout_activities(services).await;
                match result {
                    Ok(()) => break,
                    Err(error) if attempt == 1 => eprintln!("Codex 협업 기록의 마지막 갱신을 읽지 못했습니다. {error}"),
                    Err(_) => {}
                }
            }
        }
        "antigravity" => {
            // 진행 중으로 남은 카드가 없도록 마지막 상태를 확정한다.
            let result = handle.state.lock().await.consume_antigravity_reasoning(services, true).await;
            if let Err(error) = result {
                eprintln!("Antigravity 추론 기록의 마지막 갱신을 읽지 못했습니다. {error}");
            }
        }
        _ => {}
    }
}

/// consumeOutput. 한 줄씩 상태 잠금 아래에서 처리한다. 처리 오류가 나면 CLI가 막히지 않게 나머지는 읽어 버린다.
async fn consume_output(services: &RuntimeServices, handle: &TurnHandle, mut stdout: impl AsyncRead + Unpin) -> Result<(), String> {
    let mut splitter = LineSplitter::default();
    let mut failure: Option<String> = None;
    let mut buffer = vec![0_u8; 64 * 1024];
    loop {
        let count = match stdout.read(&mut buffer).await {
            Ok(0) => break,
            Ok(count) => count,
            Err(error) => {
                failure.get_or_insert(error.to_string());
                break;
            }
        };
        for line in splitter.push(&buffer[..count]) {
            if failure.is_none()
                && let Err(error) = handle.state.lock().await.consume_output_line(services, &line).await
            {
                failure = Some(error);
            }
        }
    }
    if failure.is_none()
        && let Some(line) = splitter.finish()
        && let Err(error) = handle.state.lock().await.consume_output_line(services, &line).await
    {
        failure = Some(error);
    }
    if failure.is_none()
        && let Err(error) = handle.state.lock().await.complete_pending_initial_codex_reasoning(services, None).await
    {
        failure = Some(error);
    }
    failure.map_or(Ok(()), Err)
}

/// readline(crlfDelay: Infinity)의 줄 나누기. \n, \r\n, 홀로 선 \r에서 끊고 빈 줄도 낸다. 마지막 조각은 비어 있지 않을 때만 낸다.
#[derive(Debug, Default)]
pub(crate) struct LineSplitter {
    buffer: Vec<u8>,
    skip_line_feed: bool,
}

impl LineSplitter {
    pub(crate) fn push(&mut self, chunk: &[u8]) -> Vec<String> {
        let mut lines = Vec::new();
        for &byte in chunk {
            if std::mem::take(&mut self.skip_line_feed) && byte == b'\n' {
                continue;
            }
            match byte {
                b'\n' => lines.push(self.take_line()),
                b'\r' => {
                    lines.push(self.take_line());
                    self.skip_line_feed = true;
                }
                _ => self.buffer.push(byte),
            }
        }
        lines
    }

    pub(crate) fn finish(&mut self) -> Option<String> {
        (!self.buffer.is_empty()).then(|| self.take_line())
    }

    fn take_line(&mut self) -> String {
        String::from_utf8_lossy(&std::mem::take(&mut self.buffer)).into_owned()
    }
}

/// collectStream. 조각마다 UTF-8로 풀고 20만 자(UTF-16)를 넘으면 끝부분만 남긴다.
async fn collect_stream(mut stream: impl AsyncRead + Unpin) -> String {
    let mut output = String::new();
    let mut buffer = vec![0_u8; 64 * 1024];
    while let Ok(count) = stream.read(&mut buffer).await {
        if count == 0 {
            break;
        }
        output.push_str(&String::from_utf8_lossy(&buffer[..count]));
        if js_length(&output) > STDERR_LIMIT {
            output = utf16_suffix(&output, STDERR_LIMIT);
        }
    }
    output
}

// ---- 완료·실패·중단 ----

impl Runtime {
    fn is_running(&self, handle: &Arc<TurnHandle>) -> bool {
        self.jobs().running.get(&handle.character_id).is_some_and(|current| Arc::ptr_eq(current, handle))
    }

    fn remove_running(&self, handle: &Arc<TurnHandle>) {
        let mut jobs = self.jobs();
        if jobs.running.get(&handle.character_id).is_some_and(|current| Arc::ptr_eq(current, handle)) {
            jobs.running.remove(&handle.character_id);
        }
    }

    /// complete. 응답 출처·답변·턴 상태·사용량·작업 기록·위키 수정안을 한 트랜잭션으로 저장하고, 끝나면 작업 기록의 RAG 문서를 맞춘다.
    /// 출처 검증에 실패하면 출처를 비우고 그 사유를 경고로 남긴다. Node는 새 업무에 작업 공간을 붙이지 않아(isolateGitWorkdir false)
    /// state.workspace가 늘 null이므로 완료 때의 작업 공간 검토 분기는 옮기지 않았다. 터미널 턴은 잡 표에 없는 임시 손잡이로 들어온다.
    pub(super) async fn complete(
        &self,
        services: &RuntimeServices,
        handle: &Arc<TurnHandle>,
        state: &mut TurnState,
        decoded: &Value,
    ) -> Result<(), String> {
        if handle.cancel_requested() {
            return Ok(());
        }
        state.complete_pending_initial_codex_reasoning(services, None).await?;
        // CLI가 받아 갔다는 확인이 끝내 없던 끼어들기는 반영됨으로 바꾸지 않는다.
        state.settle_pending_steers(services).await?;
        state.finalize_running_activities(services, "completed").await?;
        state.normalize_completed_message_activity(services, decoded).await?;
        let session_key = state.external_session_id.clone().unwrap_or_default();
        let image_root = services.roots.generated_images(state.backend());
        let generated: Vec<String> = list_generated_images(&session_key, &image_root)
            .map_err(|error| scandir_error(&error, &image_root, &session_key))?
            .into_iter()
            .filter(|path| !state.initial_generated_images.contains(path))
            .collect();
        let response_text = append_local_image_previews(&state.completed_response_text(decoded), &generated);
        let empty = Vec::new();
        let decoded_sources = decoded.get("sources").and_then(Value::as_array).unwrap_or(&empty);
        let response_sources = portable_response_sources(decoded_sources, &state.workdir);
        let needs_input = decoded.get("needsInput").and_then(Value::as_bool);
        let needs_input_truthy = js_truthy(decoded.get("needsInput"));
        let review_status = if needs_input_truthy { "needs_input" } else { "not_applicable" };
        let parser_warning = decoded.get("wikiProposalError").filter(|value| js_truthy(Some(value))).map(js_string);
        let record_response = js_string_or_empty(decoded.get("text"));
        let repository_root = self.repository_root.as_deref().and_then(Path::to_str).unwrap_or("").to_string();
        let ended_at = state.ended_at.map(iso_string);
        let saved: &TurnState = state;
        let completed_work_record_id = with_transaction(&services.pool, async |connection| {
            let mut source_warning = decoded.get("sourceError").and_then(pg_text);
            let stored_source_count = match replace_turn_response_sources(connection, &saved.turn_id, &response_sources).await {
                Ok(stored) => stored.len(),
                Err(SourceStoreError::Invalid(invalid)) => {
                    source_warning = Some(invalid.0);
                    replace_turn_response_sources(connection, &saved.turn_id, &[]).await.map_err(|error| match error {
                        SourceStoreError::Invalid(invalid) => AgentError::Failed(invalid.0),
                        SourceStoreError::Database(error) => AgentError::from(error),
                    })?;
                    0
                }
                Err(SourceStoreError::Database(error)) => return Err(AgentError::from(error)),
            };
            sqlx::query("UPDATE messages SET text = $2, received_at = now() WHERE turn_id = $1::uuid AND role = 'assistant'")
                .bind(&saved.turn_id)
                .bind(&response_text)
                .execute(&mut *connection)
                .await?;
            sqlx::query(
                "UPDATE turns SET status = 'completed', needs_input = $2, response_source_warning = $3,
                   wiki_proposal_warning = NULL, error_message = NULL,
                   ended_at = COALESCE($4::timestamptz, now()), updated_at = now()
                 WHERE id = $1::uuid",
            )
            .bind(&saved.turn_id)
            .bind(needs_input)
            .bind(&source_warning)
            .bind(&ended_at)
            .execute(&mut *connection)
            .await?;
            saved.persist_usage_record(services, connection).await?;
            let model = saved.resolved_model.as_deref().or(saved.character.model.as_deref());
            let work_record_id = persist_completed_turn_work_record(
                connection,
                &CompletedTurnRecord {
                    repository_root: &repository_root,
                    turn_id: &saved.turn_id,
                    workspace_id: None,
                    character_id: saved.character_id(),
                    prompt: &saved.record_prompt,
                    response: &record_response,
                    backend: Some(saved.backend()),
                    model,
                    needs_input: decoded.get("needsInput") == Some(&Value::Bool(true)),
                    response_source_count: stored_source_count,
                    response_source_warning: source_warning.as_deref(),
                    review_status: Some(review_status),
                    refresh_terminal_result: saved.refresh_terminal_result,
                    ..CompletedTurnRecord::default()
                },
            )
            .await
            .map_err(|error| AgentError::Failed(error.to_string()))?;
            let wiki_proposal_warning = persist_turn_wiki_proposals(
                connection,
                TurnWikiProposals {
                    repository_root: &repository_root,
                    work_record_id: work_record_id.as_deref(),
                    turn_id: &saved.turn_id,
                    character_id: saved.character_id(),
                    proposals: decoded.get("proposals"),
                    parser_warning: parser_warning.clone(),
                    needs_input: needs_input_truthy,
                },
            )
            .await?;
            if let Some(warning) = &wiki_proposal_warning {
                sqlx::query("UPDATE turns SET wiki_proposal_warning = $2, updated_at = now() WHERE id = $1::uuid")
                    .bind(&saved.turn_id)
                    .bind(warning)
                    .execute(&mut *connection)
                    .await?;
            }
            Ok::<_, AgentError>(work_record_id)
        })
        .await
        .map_err(|error| error.to_string())?;
        sync_work_record_rag_best_effort(services, completed_work_record_id.as_deref()).await;
        self.remove_running(handle);
        services.broadcast(json!({
            "type": "feed.changed",
            "turnId": state.turn_id,
            "characterId": state.character_id(),
            "costChanged": true,
        }));
        Ok(())
    }

    /// fail. 이 턴이 아직 실행 중일 때만 실패로 저장한다.
    /// Node는 저장이 실패하면 처리되지 않은 거부로 남지만, 여기서는 기록하고 잡 표를 비워 직원이 막히지 않게 한다.
    async fn fail(&self, services: &RuntimeServices, handle: &Arc<TurnHandle>, message: &str) {
        if !self.is_running(handle) {
            return;
        }
        let mut state = handle.state.lock().await;
        discard_structured_turn_result(state.structured_result_path.as_deref());
        state.structured_result_path = None;
        if let Err(error) = save_failure(services, &mut state, message).await {
            eprintln!("[agent-runtime] {} 실패한 업무를 저장하지 못했습니다. {error}", state.character_id());
        }
        let turn_id = state.turn_id.clone();
        drop(state);
        self.remove_running(handle);
        services.broadcast(json!({
            "type": "feed.changed",
            "turnId": turn_id,
            "characterId": handle.character_id,
            "costChanged": true,
        }));
    }

    /// 실행 중인 GUI 업무에 메시지를 끼워 넣는다. Claude는 지속 세션 입력으로, Codex는 app-server turn/steer로 보낸다.
    /// CLI가 받아들이면 접수로 답하고, 실제로 받아 갔는지는 활동 상태(steer)로 알린다. 받을 수 없으면 Busy(409)다.
    pub async fn steer(
        self: &Arc<Self>,
        character_id: &str,
        prompt: Option<&Value>,
        attachment_paths: Option<&Value>,
    ) -> Result<Value, AgentError> {
        let services = self.services()?;
        let Some(handle) = self.jobs().running.get(character_id).cloned() else {
            return Err(AgentError::JobNotFound("실행 중인 업무를 찾을 수 없습니다.".into()));
        };
        let text = match prompt {
            None | Some(Value::Null) => String::new(),
            Some(prompt) => js_trim(&js_string(prompt)).to_string(),
        };
        let attachment_paths = attachment_paths.cloned().unwrap_or_else(|| json!([]));
        if text.is_empty() && attachment_list_is_empty(&attachment_paths)? {
            return Err(AgentError::Failed("끼어들 내용을 입력하세요.".into()));
        }
        let (claude, codex) = (handle.claude_worker(), handle.codex_steer());
        if handle.cancel_requested() || (claude.is_none() && codex.is_none()) {
            return Err(AgentError::Busy(STEER_UNSUPPORTED.into()));
        }
        let record_text = if text.is_empty() { "첨부 파일을 확인해줘.".to_string() } else { text };
        let steer_id = random_uuid();
        let (prompt_text, images, attachments) = {
            let mut state = handle.state.lock().await;
            let attachments = stage_attachments(&attachment_paths, &state.workdir).map_err(AgentError::Failed)?;
            if let Err(error) = state.begin_steer(&services, &steer_id, &record_text).await {
                // 기록하지 못하면 CLI에 보내지 않는다. 앱이 원본으로 다시 보내므로 이번 복사본은 남기지 않는다.
                remove_staged_attachments(&attachments);
                return Err(AgentError::Failed(error));
            }
            let images: Vec<String> =
                attachments.iter().filter(|attachment| attachment.is_codex_image).map(|attachment| attachment.path.clone()).collect();
            (prompt_with_attachments(&record_text, &attachments), images, attachments)
        };
        let sent = match (claude, codex) {
            (Some(worker), _) => worker.steer(&steer_id, &prompt_text).await,
            (None, Some(codex)) => {
                let (reply, answer) = oneshot::channel();
                let steer = CodexSteer { steer_id: steer_id.clone(), input: codex_turn_input(&prompt_text, &images), reply };
                match codex.send(steer) {
                    Ok(()) => answer.await.unwrap_or_else(|_| Err(STEER_UNSUPPORTED.into())),
                    Err(_) => Err(STEER_UNSUPPORTED.into()),
                }
            }
            (None, None) => Err(STEER_UNSUPPORTED.into()),
        };
        if let Err(error) = sent {
            remove_staged_attachments(&attachments);
            let mut state = handle.state.lock().await;
            if let Err(saved) = state.finish_steer(&services, &steer_id, false).await {
                eprintln!("[agent-runtime] {character_id} 끼어들기 실패를 저장하지 못했습니다. {saved}");
            }
            eprintln!("[agent-runtime] {character_id} 끼어들기를 전달하지 못했습니다. {error}");
            return Err(AgentError::Busy(STEER_UNSUPPORTED.into()));
        }
        Ok(json!({ "turnId": handle.turn_id, "steerId": steer_id, "status": "accepted" }))
    }

    /// cancel. 응답을 기다리던 요청이 끊겨도 중단 저장이 멈추지 않도록 별도 작업에서 돈다.
    pub async fn cancel(self: &Arc<Self>, character_id: &str) -> Result<Value, AgentError> {
        let runtime = Arc::clone(self);
        let character_id = character_id.to_string();
        tokio::spawn(async move { runtime.cancel_job(&character_id).await })
            .await
            .unwrap_or_else(|join_error| Err(AgentError::Failed(join_error.to_string())))
    }

    async fn cancel_job(&self, character_id: &str) -> Result<Value, AgentError> {
        let services = self.services()?;
        let handle = self.jobs().running.get(character_id).cloned();
        let Some(handle) = handle else {
            return Err(AgentError::JobNotFound("실행 중인 업무를 찾을 수 없습니다.".into()));
        };
        handle.cancel_requested.store(true, Ordering::SeqCst);
        if let Some(worker) = handle.claude_worker() {
            // 워커는 중단 result를 받을 때까지 기다리므로, 이 뒤의 사용량 저장에는 Claude Code가 보고한 비용이 들어간다.
            worker.cancel_current().await;
        } else if let Some(process) = handle.process() {
            process.terminate(FORCE_KILL_AFTER, false);
        }
        let mut state = handle.state.lock().await;
        let saved = save_interruption(&services, &mut state, "사용자가 업무를 중단했습니다.").await;
        discard_structured_turn_result(state.structured_result_path.as_deref());
        state.structured_result_path = None;
        let turn_id = state.turn_id.clone();
        drop(state);
        self.remove_running(&handle);
        services.broadcast(json!({
            "type": "feed.changed",
            "turnId": turn_id,
            "characterId": character_id,
            "costChanged": true,
        }));
        saved?;
        Ok(json!({ "turnId": turn_id, "status": "interrupted" }))
    }

    /// recordDeveloperToolEvent. 실행 중인 GUI 턴, 또는 열린 터미널의 실행 중 턴의 같은 세션에서 온 도구 이벤트만 활동으로 받는다.
    pub async fn record_developer_tool_event(self: &Arc<Self>, body: Value) -> Result<Value, String> {
        let runtime = Arc::clone(self);
        tokio::spawn(async move { runtime.record_developer_tool_event_job(&body).await })
            .await
            .unwrap_or_else(|join_error| Err(join_error.to_string()))
    }

    async fn record_developer_tool_event_job(&self, body: &Value) -> Result<Value, String> {
        let Some(event) = parse_developer_tool_event(body) else {
            return Ok(json!({ "accepted": false, "reason": "invalid_event" }));
        };
        let inactive = json!({ "accepted": false, "reason": "inactive_session" });
        if event.mode != "gui" {
            let Some(terminals) = self.terminal_sessions() else {
                return Ok(inactive);
            };
            let services = self.services().map_err(|error| error.to_string())?;
            return terminals.record_tool_event(&services, &event).await;
        }
        let handle = self.jobs().running.get(&event.character_id).cloned();
        let Some(handle) = handle else {
            return Ok(inactive);
        };
        let services = self.services().map_err(|error| error.to_string())?;
        let mut state = handle.state.lock().await;
        if handle.cancel_requested() || state.external_session_id.as_deref() != Some(event.session_id.as_str()) {
            return Ok(inactive);
        }
        if !state.developer_tool_event_keys.admit(&event.event_key) {
            return Ok(json!({ "accepted": false, "reason": "event_limit" }));
        }
        state.add_activity(&services, &event.activity()).await?;
        Ok(json!({ "accepted": true }))
    }
}

/// fail의 저장 묶음.
async fn save_failure(services: &RuntimeServices, state: &mut TurnState, message: &str) -> Result<(), String> {
    state.complete_pending_initial_codex_reasoning(services, None).await?;
    state.finalize_running_activities(services, "failed").await?;
    let saved: &TurnState = state;
    with_transaction(&services.pool, async |connection| {
        sqlx::query(
            "UPDATE turns SET status = 'failed', error_message = $2, ended_at = now(), updated_at = now() WHERE id = $1::uuid",
        )
        .bind(&saved.turn_id)
        .bind(message)
        .execute(&mut *connection)
        .await?;
        saved.persist_usage_record(services, connection).await?;
        if saved.external_session_id.as_deref().is_none_or(str::is_empty) {
            sqlx::query("UPDATE cli_sessions SET ended_at = COALESCE(ended_at, now()) WHERE id = $1::uuid")
                .bind(&saved.session_id)
                .execute(&mut *connection)
                .await?;
        }
        Ok::<_, AgentError>(())
    })
    .await
    .map_err(|error| error.to_string())
}

/// cancel의 저장 묶음. Node의 state.workspace는 늘 null이라 작업 공간 실패 처리 분기는 옮기지 않았다.
async fn save_interruption(services: &RuntimeServices, state: &mut TurnState, message: &str) -> Result<(), AgentError> {
    state.complete_pending_initial_codex_reasoning(services, None).await?;
    state.finalize_running_activities(services, "failed").await?;
    sqlx::query(
        "UPDATE turns SET status = 'interrupted', needs_input = false, error_message = $2, ended_at = now(), updated_at = now()
         WHERE id = $1::uuid",
    )
    .bind(&state.turn_id)
    .bind(message)
    .execute(&services.pool)
    .await?;
    recover_interrupted_usage(services, state);
    let mut connection = services.pool.acquire().await?;
    state.persist_usage_record(services, &mut connection).await?;
    Ok(())
}

/// recoverInterruptedUsage. 중단돼 사용량 이벤트를 못 받았으면 CLI가 남긴 세션 기록에서 마지막 사용량을 읽는다.
fn recover_interrupted_usage(services: &RuntimeServices, state: &mut TurnState) {
    if state.usage.is_some() {
        return;
    }
    let Some(session_id) = state.external_session_id.clone().filter(|id| !id.is_empty()) else {
        return;
    };
    let roots = &services.roots;
    let recorded = match state.backend() {
        "codex" => latest_codex_usage_from_rollout(find_rollout_path(&roots.codex_sessions(), &session_id).as_deref()),
        "antigravity" => antigravity_session_usage(&session_id, &roots.antigravity_conversations()),
        "claude" => latest_claude_usage_from_session(
            claude_usage_session_path(&roots.claude_projects(), Some(&state.workdir), &session_id).as_deref(),
        ),
        _ => None,
    };
    if let Some(usage) = recorded.and_then(|usage| state.usage_for_turn(usage)) {
        state.usage = Some(usage);
    }
}

// ---- CLI 전환 전 세션 종료 ----

impl Runtime {
    /// inspectWorkspaceForSessionEnd. 실행 중이거나 대기 턴이 있으면 막는다.
    /// 활성 작업 공간이 있으면 최근 업무 턴과 지금 변경 검토 결과를 계획에 담는다.
    pub async fn inspect_workspace_for_session_end(
        &self,
        character_id: &str,
        connection: &mut PgConnection,
    ) -> Result<SessionEndPlan, CharacterSettingsError> {
        if self.jobs().running.contains_key(character_id) {
            return Err(CharacterSettingsError::Busy(SESSION_END_BUSY.into()));
        }
        let busy: Option<String> = sqlx::query_scalar(
            "SELECT turn.id::text FROM turns AS turn
             JOIN cli_sessions AS session ON session.id = turn.cli_session_id
             WHERE session.character_id = $1 AND turn.status IN ('pending', 'running')
             LIMIT 1",
        )
        .bind(character_id)
        .fetch_optional(&mut *connection)
        .await?;
        if busy.is_some() {
            return Err(CharacterSettingsError::Busy(SESSION_END_BUSY.into()));
        }
        let candidate = sqlx::query(AssertSqlSafe(format!(
            "SELECT {}, latest_turn.id::text AS review_candidate_turn_id
             FROM task_workspaces AS workspace
             JOIN cli_sessions AS session ON session.id = workspace.cli_session_id
             LEFT JOIN LATERAL (
               SELECT turn.id FROM turns AS turn
               WHERE turn.task_workspace_id = workspace.id
               ORDER BY turn.started_at DESC, turn.id DESC
               LIMIT 1
             ) AS latest_turn ON true
             WHERE session.character_id = $1
               AND workspace.status IN ('active', 'awaiting_approval', 'merging', 'conflict')
             ORDER BY CASE workspace.status
                 WHEN 'awaiting_approval' THEN 1 WHEN 'merging' THEN 2 WHEN 'conflict' THEN 3 ELSE 4
               END,
               workspace.updated_at DESC
             LIMIT 1",
            workspace_columns("workspace")
        )))
        .bind(character_id)
        .fetch_optional(&mut *connection)
        .await?;
        let Some(candidate) = candidate else {
            return Ok(SessionEndPlan {
                character_id: character_id.to_string(),
                workspace: None,
                review_turn_id: None,
                review: None,
            });
        };
        let workspace = workspace_from_row(&candidate)?;
        if BLOCKED_WORKSPACE_STATUSES.contains(&workspace.status.as_str()) {
            return Err(CharacterSettingsError::Busy("이 직원의 대기 중 변경사항을 먼저 통합하거나 폐기하세요.".into()));
        }
        let Some(manager) = self.services().ok().and_then(|services| services.workspace_manager.clone()) else {
            return Err(CharacterSettingsError::Busy("작업 공간 상태를 확인할 Git 실행기가 준비되지 않았습니다.".into()));
        };
        let review_turn_id: Option<String> = candidate.try_get("review_candidate_turn_id")?;
        let Some(review_turn_id) = review_turn_id else {
            return Err(CharacterSettingsError::Busy("작업 공간에 연결된 업무 기록을 찾을 수 없습니다.".into()));
        };
        let review = manager.prepare_review(&workspace).await?;
        Ok(SessionEndPlan {
            character_id: character_id.to_string(),
            workspace: Some(workspace),
            review_turn_id: Some(review_turn_id),
            review: Some(review),
        })
    }

    /// applyWorkspaceSessionEndPlan. 작업 공간 없는 계획이면 활성 세션을 지우고 열린 세션을 모두 끝낸다.
    /// 변경 없는 작업 공간은 닫고 그 세션만 끝낸다.
    pub async fn apply_workspace_session_end_plan(
        &self,
        connection: &mut PgConnection,
        plan: &SessionEndPlan,
    ) -> Result<(), CharacterSettingsError> {
        if plan.has_review_changes() {
            return Err(CharacterSettingsError::Busy(
                "CLI를 변경하기 전에 현재 작업 공간의 변경사항을 통합하거나 폐기하세요.".into(),
            ));
        }
        let Some(workspace) = &plan.workspace else {
            sqlx::query("DELETE FROM active_cli_sessions WHERE character_id = $1")
                .bind(&plan.character_id)
                .execute(&mut *connection)
                .await?;
            sqlx::query(
                "UPDATE cli_sessions SET ended_at = COALESCE(ended_at, now()) WHERE character_id = $1 AND ended_at IS NULL",
            )
            .bind(&plan.character_id)
            .execute(&mut *connection)
            .await?;
            return Ok(());
        };
        let closed: Option<String> = sqlx::query_scalar(
            "UPDATE task_workspaces
             SET status = 'closed', review_tree = NULL, changed_files = '[]'::jsonb, error_message = NULL, updated_at = now()
             WHERE id = $1::uuid AND status = 'active'
             RETURNING id::text",
        )
        .bind(&workspace.id)
        .fetch_optional(&mut *connection)
        .await?;
        if closed.is_none() {
            return Err(CharacterSettingsError::Busy("작업 공간 상태가 바뀌었습니다. 다시 확인하세요.".into()));
        }
        sqlx::query("DELETE FROM active_cli_sessions WHERE cli_session_id = $1::uuid")
            .bind(&workspace.cli_session_id)
            .execute(&mut *connection)
            .await?;
        sqlx::query("UPDATE cli_sessions SET ended_at = COALESCE(ended_at, now()) WHERE id = $1::uuid")
            .bind(&workspace.cli_session_id)
            .execute(&mut *connection)
            .await?;
        Ok(())
    }

    /// finalizeWorkspaceSessionEndPlan. 닫은 작업 공간의 worktree를 지우고 작업 기록을 검토 불필요로 둔다.
    /// 실패는 경고 문구로만 돌려준다.
    pub async fn finalize_workspace_session_end_plan(&self, plan: &SessionEndPlan) -> Result<Option<String>, String> {
        let Some(workspace) = &plan.workspace else {
            return Ok(None);
        };
        let services = self.services().map_err(|error| error.to_string())?;
        let mut warnings = Vec::new();
        if let Some(manager) = &services.workspace_manager
            && let Err(error) = manager.cleanup(workspace).await
        {
            warnings.push(format!("빈 작업 공간 정리에 실패했습니다. {}", error.message));
        }
        let review_turn_id = plan.review_turn_id.clone().unwrap_or_default();
        let transition_warning = transition_work_record_review_best_effort(
            &services,
            ReviewTransition {
                turn_id: review_turn_id.clone(),
                status: "not_required".into(),
                review_tree: plan.review.as_ref().map(|review| review.review_tree.clone()),
                head_commit: plan.review.as_ref().map(|review| review.head_commit.clone()),
                changed_files: Some(json!([])),
                actor_type: Some("system".into()),
                ..ReviewTransition::default()
            },
        )
        .await;
        warnings.extend(transition_warning);
        services.broadcast(json!({
            "type": "workspace.changed",
            "turnId": review_turn_id,
            "characterId": plan.character_id,
            "status": "closed",
        }));
        Ok((!warnings.is_empty()).then(|| format!("{}: {}", plan.character_id, warnings.join(" "))))
    }

    /// prepareWorkspaceForSessionEnd. 한 직원의 CLI를 바꾸기 전에 지속 세션을 닫고 활성 세션을 끝낸다.
    /// 작업 공간에 변경이 있으면 승인 대기로 돌리고 막으며, 변경이 없으면 worktree를 지우고 공간을 닫는다.
    pub async fn prepare_workspace_for_session_end(&self, character_id: &str) -> Result<(), CharacterSettingsError> {
        let Ok(services) = self.services() else {
            return Err(CharacterSettingsError::Busy("CLI 실행기가 준비된 뒤 설정을 변경하세요.".into()));
        };
        let plan = {
            let mut connection = services.pool.acquire().await?;
            self.inspect_workspace_for_session_end(character_id, &mut connection).await?
        };
        let (Some(workspace), Some(review)) = (&plan.workspace, &plan.review) else {
            self.close_claude_worker(character_id, None, "직원 CLI 세션을 종료합니다.");
            return with_transaction(&services.pool, async |connection| {
                self.apply_workspace_session_end_plan(connection, &plan).await
            })
            .await;
        };
        let review_turn_id = plan.review_turn_id.clone().unwrap_or_default();
        if review.has_changes {
            self.close_claude_worker(character_id, None, "작업 공간 검토 전에 Claude Code 세션을 닫습니다.");
            let changed_files = Value::Array(review.changed_files.clone());
            let updated: Option<String> = sqlx::query_scalar(
                "UPDATE task_workspaces
                 SET status = 'awaiting_approval', review_turn_id = $2::uuid, review_tree = $3, head_commit = $4,
                     changed_files = $5::jsonb, error_message = NULL, review_requested_at = now(), updated_at = now()
                 WHERE id = $1::uuid AND status = 'active'
                 RETURNING id::text",
            )
            .bind(&workspace.id)
            .bind(&review_turn_id)
            .bind(&review.review_tree)
            .bind(&review.head_commit)
            .bind(changed_files.to_string())
            .fetch_optional(&services.pool)
            .await?;
            if updated.is_none() {
                return Err(CharacterSettingsError::Busy("작업 공간 상태가 바뀌었습니다. 다시 확인하세요.".into()));
            }
            transition_work_record_review_best_effort(
                &services,
                ReviewTransition {
                    turn_id: review_turn_id.clone(),
                    status: "awaiting_approval".into(),
                    review_tree: Some(review.review_tree.clone()),
                    head_commit: Some(review.head_commit.clone()),
                    changed_files: Some(changed_files),
                    actor_type: Some("system".into()),
                    ..ReviewTransition::default()
                },
            )
            .await;
            services.broadcast(json!({
                "type": "workspace.changed",
                "turnId": review_turn_id,
                "characterId": character_id,
                "status": "awaiting_approval",
            }));
            services.broadcast(json!({ "type": "feed.changed", "turnId": review_turn_id, "characterId": character_id }));
            return Err(CharacterSettingsError::Busy(
                "CLI를 변경하기 전에 현재 작업 공간의 변경사항을 통합하거나 폐기하세요.".into(),
            ));
        }
        self.close_claude_worker(character_id, None, "직원 CLI 세션을 종료합니다.");
        // 계획에 작업 공간이 있으면 inspect가 관리자를 이미 확인했다.
        if let Some(manager) = &services.workspace_manager {
            manager.cleanup(workspace).await?;
        }
        with_transaction(&services.pool, async |connection| self.apply_workspace_session_end_plan(connection, &plan).await)
            .await?;
        transition_work_record_review_best_effort(
            &services,
            ReviewTransition {
                turn_id: review_turn_id.clone(),
                status: "not_required".into(),
                review_tree: Some(review.review_tree.clone()),
                head_commit: Some(review.head_commit.clone()),
                changed_files: Some(json!([])),
                actor_type: Some("system".into()),
                ..ReviewTransition::default()
            },
        )
        .await;
        services.broadcast(json!({
            "type": "workspace.changed",
            "turnId": review_turn_id,
            "characterId": character_id,
            "status": "closed",
        }));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn split(chunks: &[&[u8]]) -> Vec<String> {
        let mut splitter = LineSplitter::default();
        let mut lines: Vec<String> = chunks.iter().flat_map(|chunk| splitter.push(chunk)).collect();
        lines.extend(splitter.finish());
        lines
    }

    #[test]
    fn 줄_나누기는_node_readline과_같다() {
        assert_eq!(split(&[b"a\nb\r\nc\rd"]), ["a", "b", "c", "d"]);
        assert_eq!(split(&[b"a\n\nb\n"]), ["a", "", "b"]);
        // \r 뒤 \n이 다음 조각에 와도 줄바꿈 하나다.
        assert_eq!(split(&[b"a\r", b"\nb"]), ["a", "b"]);
        assert_eq!(split(&[b"a\r", b"\r", b"b"]), ["a", "", "b"]);
        assert_eq!(split(&[b""]), Vec::<String>::new());
        assert_eq!(split(&[b"\n"]), [""]);
        // 여러 바이트 글자가 조각 사이에서 갈려도 한 줄로 풀린다.
        let text = "직원\n".as_bytes();
        assert_eq!(split(&[&text[..2], &text[2..]]), ["직원"]);
    }

    #[test]
    fn 첨부_목록_비었는지는_length로_본다() {
        assert_eq!(attachment_list_is_empty(&json!([])).ok(), Some(true));
        assert_eq!(attachment_list_is_empty(&json!(["a"])).ok(), Some(false));
        assert_eq!(attachment_list_is_empty(&json!("")).ok(), Some(true));
        assert_eq!(attachment_list_is_empty(&json!({ "length": 0 })).ok(), Some(true));
        assert_eq!(attachment_list_is_empty(&json!({})).ok(), Some(false));
        assert_eq!(attachment_list_is_empty(&json!(0)).ok(), Some(false));
        assert_eq!(
            attachment_list_is_empty(&Value::Null).err().map(|error| error.to_string()),
            Some("Cannot read properties of null (reading 'length')".to_string())
        );
    }

    #[tokio::test]
    async fn 표준_오류는_끝부분_20만_자만_남긴다() {
        let mut text = "가".repeat(100);
        text.push_str(&"a".repeat(STDERR_LIMIT));
        let collected = collect_stream(text.as_bytes()).await;
        assert_eq!(js_length(&collected), STDERR_LIMIT);
        assert!(collected.chars().all(|ch| ch == 'a'));
        assert_eq!(collect_stream(&b"  err \n"[..]).await, "  err \n");
    }

    #[test]
    fn 세션_비교는_저장소_루트가_먼저다() {
        let row = |conversation: Option<&str>, session_root: Option<&str>| ActiveSession {
            id: "s".into(),
            external_session_id: None,
            conversation_id: "c".into(),
            conversation_workdir: conversation.map(str::to_string),
            session_repository_root: session_root.map(str::to_string),
            workspace_repository_root: None,
            resume_execution_workdir: None,
        };
        let root = std::env::temp_dir().join(format!("lifecycle-match-{}", std::process::id()));
        let work = root.join("work");
        std::fs::create_dir_all(&work).unwrap();
        let root_text = root.to_str().unwrap();
        let work_text = work.to_str().unwrap();
        assert!(active_session_matches_runtime(&row(Some(work_text), None), Some(work_text), Some(root_text)));
        assert!(active_session_matches_runtime(&row(Some(root_text), None), Some(work_text), Some(root_text)));
        assert!(!active_session_matches_runtime(&row(Some("/elsewhere"), None), Some(work_text), Some(root_text)));
        assert!(!active_session_matches_runtime(&row(Some(" "), None), Some(work_text), Some(root_text)));
        // 저장소 루트가 있으면 대화 폴더가 같아도 루트로만 비교한다.
        assert!(!active_session_matches_runtime(&row(Some(work_text), Some("/other")), Some(work_text), Some(root_text)));
        assert!(active_session_matches_runtime(&row(None, Some(&format!("{root_text}/work/.."))), Some(work_text), Some(root_text)));
        std::fs::remove_dir_all(&root).unwrap();
    }
}
