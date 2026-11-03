// 터미널 관리자가 실행기에 맡기는 DB 일: 세션 준비·외부 세션 묶기·턴 시작·완료·비용·중단(Node AgentRuntime 터미널 메서드).

use std::path::Path;

use serde_json::{Value, json};
use sqlx::{PgConnection, Row};

use super::claude_session::{
    claude_session_file_matches, claude_session_path, find_claude_session_path, prepare_claude_session_resume,
};
use super::launch::RuntimeCharacter;
use super::lifecycle::{ActiveSession, active_session_matches_runtime};
use super::rollout::find_rollout_path;
use super::service::{AgentError, RuntimeServices};
use super::turn::{TurnHandle, TurnSeed, TurnState};
use super::usage::empty_usage;
use crate::agent_event_parser::decode_agent_response;
use crate::db::with_transaction;
use crate::health::Runtime;
use crate::js::{iso_string, js_trim, now_milliseconds, number_to_string, pg_text, random_uuid};
use crate::local_provider::service::LocalProviderService;
use crate::local_provider::{resolve_local_character, snapshot_local_turn, validate_local_resume};
use crate::reply_delivery::{ReplyDeliveryError, record_reply_delivery};
use crate::structured_turn_result::apply_structured_turn_result;
use crate::terminal_sessions::{TerminalLaunch, TerminalRuntime, TerminalTurnCompletion, TerminalTurnStart};
use crate::terminal_turn_activities::is_terminal_final_message;
use crate::terminal_usage::{CodexScope, TurnWindow, claude_transcript_turn, codex_rollout_turn_usage};

const BUSY_TURN_SQL: &str = "SELECT turn.id::text FROM turns AS turn
     JOIN cli_sessions AS session ON session.id = turn.cli_session_id
     WHERE session.character_id = $1 AND turn.status IN ('pending', 'running')
     LIMIT 1";

const UPSERT_ACTIVE_SQL: &str = "INSERT INTO active_cli_sessions (character_id, cli_session_id) VALUES ($1, $2::uuid)
     ON CONFLICT (character_id) DO UPDATE SET cli_session_id = EXCLUDED.cli_session_id, updated_at = now()";

fn reply_delivery_failure(error: ReplyDeliveryError) -> AgentError {
    match error {
        ReplyDeliveryError::Database(error) => AgentError::from(error),
        other => AgentError::Failed(other.to_string()),
    }
}

async fn lock_character(connection: &mut PgConnection, character_id: &str) -> Result<(), AgentError> {
    sqlx::query("SELECT pg_advisory_xact_lock(hashtext($1))")
        .bind(format!("officestra:character:{character_id}"))
        .execute(&mut *connection)
        .await?;
    Ok(())
}

async fn character_has_open_turn(connection: &mut PgConnection, character_id: &str) -> Result<bool, AgentError> {
    let busy: Option<String> = sqlx::query_scalar(BUSY_TURN_SQL).bind(character_id).fetch_optional(&mut *connection).await?;
    Ok(busy.is_some())
}

/// prepareTerminalLaunch의 활성 세션 조회. 터미널은 저장소 루트를 세션 작업 공간에서만 읽는다(진행 중 작업 공간 열 없음).
async fn terminal_active_session(connection: &mut PgConnection, character_id: &str) -> Result<Option<ActiveSession>, AgentError> {
    let row = sqlx::query(
        "SELECT session.id::text AS id, session.external_id, session.conversation_id::text AS conversation_id,
           conversation.workdir AS conversation_workdir,
           session_scope.repository_root AS session_repository_root,
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
           SELECT candidate.execution_workdir FROM turns AS completed_turn
           JOIN task_workspaces AS candidate ON candidate.id = completed_turn.task_workspace_id
           WHERE completed_turn.cli_session_id = session.id AND completed_turn.status = 'completed'
             AND candidate.execution_workdir IS NOT NULL
           ORDER BY completed_turn.ended_at DESC NULLS LAST
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
        workspace_repository_root: None,
        resume_execution_workdir: row.try_get("resume_execution_workdir")?,
    }))
}

impl TerminalRuntime for Runtime {
    fn is_draining(&self) -> bool {
        Runtime::is_draining(self)
    }

    fn character_busy(&self, character_id: &str) -> bool {
        let jobs = self.jobs();
        jobs.running.contains_key(character_id)
            || jobs.compacting.contains(character_id)
            || jobs.preparing_characters.contains(character_id)
    }

    fn is_compacting(&self, character_id: &str) -> bool {
        self.jobs().compacting.contains(character_id)
    }

    fn broadcast(&self, event: Value) {
        if let Ok(services) = self.services() {
            services.broadcast(event);
        }
    }

    async fn prepare_terminal_launch(&self, character_id: &str) -> Result<TerminalLaunch, AgentError> {
        let id = js_trim(character_id).to_string();
        if Runtime::is_draining(self) {
            return Err(AgentError::Draining("백엔드가 안전한 전환을 준비 중이라 터미널을 열 수 없습니다.".into()));
        }
        if id.is_empty() || self.character_busy(&id) {
            return Err(AgentError::Busy("이 직원의 현재 업무가 끝난 뒤 터미널을 여세요.".into()));
        }
        let services = self.services()?;
        let workdir = self.workdir_text().ok_or(AgentError::Unavailable)?;
        let repository_root = self.repository_root.as_deref().and_then(Path::to_str).map(str::to_string);
        let (launch, resume_execution_workdir) = with_transaction(&services.pool, async |connection| {
            lock_character(connection, &id).await?;
            let Some(row) = sqlx::query(
                "SELECT id, name, seat, backend, model, effort, fast_mode, auto_compact_percent, permission,
                   identity_prompt, config
                 FROM characters WHERE id = $1 FOR UPDATE",
            )
            .bind(&id)
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
            if character_has_open_turn(connection, &id).await? {
                return Err(AgentError::Busy("이 직원은 이미 업무를 처리하고 있습니다.".into()));
            }
            let active = terminal_active_session(connection, &id)
                .await?
                .filter(|row| active_session_matches_runtime(row, Some(&workdir), repository_root.as_deref()));
            let (session_id, conversation_id, external_session_id, resume) = match active {
                Some(active) => {
                    (active.id, active.conversation_id, active.external_session_id, active.resume_execution_workdir)
                }
                None => {
                    let conversation_id = random_uuid();
                    sqlx::query("INSERT INTO conversations (id, title, workdir) VALUES ($1::uuid, '터미널 대화', $2)")
                        .bind(&conversation_id)
                        .bind(&workdir)
                        .execute(&mut *connection)
                        .await?;
                    let previous: Option<String> = sqlx::query_scalar(
                        "SELECT id::text FROM cli_sessions WHERE character_id = $1 ORDER BY started_at DESC, id DESC LIMIT 1",
                    )
                    .bind(&id)
                    .fetch_optional(&mut *connection)
                    .await?;
                    let session_id: String = sqlx::query_scalar(
                        "INSERT INTO cli_sessions (conversation_id, character_id, previous_session_id)
                         VALUES ($1::uuid, $2, $3::uuid) RETURNING id::text",
                    )
                    .bind(&conversation_id)
                    .bind(&id)
                    .bind(previous)
                    .fetch_one(&mut *connection)
                    .await?;
                    sqlx::query(UPSERT_ACTIVE_SQL).bind(&id).bind(&session_id).execute(&mut *connection).await?;
                    (session_id, conversation_id, None, None)
                }
            };
            // 로컬 모델 직원은 정의를 붙이고, 이어 쓸 세션이 같은 로컬 정의로 시작됐는지 확인한다. 실패하면 트랜잭션은 버려진다.
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
            let launch = TerminalLaunch { character, session_id, conversation_id, external_session_id, workdir: workdir.clone() };
            Ok((launch, resume))
        })
        .await?;
        if launch.character.backend == "claude" {
            self.close_claude_worker(&id, None, "터미널 모드가 같은 Claude Code 세션을 이어갑니다.");
            if let Some(external) = launch.external_session_id.as_deref().filter(|external| !external.is_empty()) {
                prepare_claude_session_resume(
                    &services.roots.claude_projects(),
                    external,
                    Some(&launch.workdir),
                    resume_execution_workdir.as_deref(),
                )?;
            }
        }
        Ok(launch)
    }

    async fn bind_terminal_external_session(
        &self,
        character_id: &str,
        session_id: &str,
        external_session_id: &str,
    ) -> Result<(), AgentError> {
        let external = js_trim(external_session_id).to_string();
        if external.is_empty() {
            return Err(AgentError::Failed("CLI 세션 ID가 비어 있습니다.".into()));
        }
        let services = self.services()?;
        with_transaction(&services.pool, async |connection| {
            lock_character(connection, character_id).await?;
            let updated: Option<String> = sqlx::query_scalar(
                "UPDATE cli_sessions SET external_id = $3, ended_at = NULL
                 WHERE id = $1::uuid AND character_id = $2 AND (external_id IS NULL OR external_id = $3)
                 RETURNING id::text",
            )
            .bind(session_id)
            .bind(character_id)
            .bind(&external)
            .fetch_optional(&mut *connection)
            .await?;
            if updated.is_none() {
                return Err(AgentError::SessionNotFound("터미널의 CLI 세션을 현재 직원에게 연결할 수 없습니다.".into()));
            }
            sqlx::query(
                "UPDATE cli_sessions SET ended_at = COALESCE(ended_at, now())
                 WHERE character_id = $1 AND id <> $2::uuid AND ended_at IS NULL",
            )
            .bind(character_id)
            .bind(session_id)
            .execute(&mut *connection)
            .await?;
            sqlx::query(UPSERT_ACTIVE_SQL).bind(character_id).bind(session_id).execute(&mut *connection).await?;
            Ok::<_, AgentError>(())
        })
        .await?;
        services.broadcast(json!({ "type": "session.changed", "characterId": character_id }));
        Ok(())
    }

    async fn begin_terminal_turn(&self, start: TerminalTurnStart) -> Result<String, AgentError> {
        let prompt = js_trim(&start.prompt).to_string();
        if prompt.is_empty() {
            return Err(AgentError::Failed("터미널 프롬프트가 비어 있습니다.".into()));
        }
        let services = self.services()?;
        let turn_id = random_uuid();
        let started_at = iso_string(start.started_at.unwrap_or_else(now_milliseconds));
        with_transaction(&services.pool, async |connection| {
            let session: Option<String> = sqlx::query_scalar(
                "SELECT session.id::text FROM cli_sessions AS session
                 JOIN characters AS character ON character.id = session.character_id
                 WHERE session.id = $1::uuid AND session.character_id = $2
                 FOR UPDATE OF session, character",
            )
            .bind(&start.session_id)
            .bind(&start.character_id)
            .fetch_optional(&mut *connection)
            .await?;
            if session.is_none() {
                return Err(AgentError::SessionNotFound("터미널 세션을 찾을 수 없습니다.".into()));
            }
            if character_has_open_turn(connection, &start.character_id).await? {
                return Err(AgentError::Busy("터미널의 이전 응답이 아직 끝나지 않았습니다.".into()));
            }
            let settings = &start.execution;
            sqlx::query(
                "INSERT INTO turns (id, cli_session_id, backend, model, effort, fast_mode,
                   origin, prompt, status, started_at, updated_at, sender_character_id, request_source)
                 VALUES ($1::uuid, $2::uuid, $3, $4, $5, $6, 'terminal', $7, 'running', $8::timestamptz, now(), $9, $10)",
            )
            .bind(&turn_id)
            .bind(&start.session_id)
            .bind(&settings.backend)
            .bind(&settings.model)
            .bind(&settings.effort)
            .bind(settings.fast_mode)
            .bind(&prompt)
            .bind(&started_at)
            .bind(start.sender_character_id.as_ref().and_then(pg_text))
            .bind(start.request_source.as_deref())
            .execute(&mut *connection)
            .await?;
            sqlx::query("INSERT INTO messages (turn_id, role, text) VALUES ($1::uuid, 'user', $2), ($1::uuid, 'assistant', '')")
                .bind(&turn_id)
                .bind(&prompt)
                .execute(&mut *connection)
                .await?;
            snapshot_local_turn(connection, &turn_id, settings.local_profile.as_ref()).await?;
            record_reply_delivery(connection, &turn_id, &start.character_id, start.delivery_id.as_deref())
                .await
                .map_err(reply_delivery_failure)?;
            Ok(())
        })
        .await?;
        services.broadcast(json!({ "type": "feed.changed", "turnId": turn_id, "characterId": start.character_id }));
        Ok(turn_id)
    }

    async fn complete_terminal_turn(&self, completion: TerminalTurnCompletion) -> Result<(), AgentError> {
        let TerminalTurnCompletion {
            character_id,
            turn_id,
            response,
            ended_at,
            usage,
            resolved_model,
            initial_generated_images,
            structured,
            reported_cost_usd,
            activities,
            refresh_completed,
            restore_tool_activities,
        } = completion;
        let services = self.services()?;
        let row = sqlx::query(
            "SELECT turn.cli_session_id::text AS session_id, turn.prompt,
               turn.backend AS execution_backend, turn.model AS execution_model,
               turn.effort AS execution_effort, turn.fast_mode AS execution_fast_mode,
               floor(extract(epoch FROM turn.started_at) * 1000)::float8 AS started_at,
               session.conversation_id::text AS conversation_id, session.external_id,
               character.id AS character_id, character.name, character.seat, character.auto_compact_percent,
               character.permission, character.identity_prompt, character.config,
               turn.provider_kind, turn.provider_snapshot
             FROM turns AS turn
             JOIN cli_sessions AS session ON session.id = turn.cli_session_id
             JOIN characters AS character ON character.id = session.character_id
             WHERE turn.id = $1::uuid AND session.character_id = $2 AND turn.origin = 'terminal'
               AND (turn.status = 'running' OR ($3::boolean AND turn.backend = 'antigravity' AND turn.status = 'completed'))",
        )
        .bind(&turn_id)
        .bind(&character_id)
        .bind(refresh_completed)
        .fetch_optional(&services.pool)
        .await?;
        let Some(row) = row else {
            return Err(AgentError::JobNotFound("완료할 터미널 턴을 찾을 수 없습니다.".into()));
        };
        let execution_backend: Option<String> = row.try_get("execution_backend")?;
        if refresh_completed && execution_backend.as_deref() != Some("antigravity") {
            return Err(AgentError::JobNotFound("Antigravity 터미널 후속 응답만 갱신할 수 있습니다.".into()));
        }
        let decoded = apply_structured_turn_result(&decode_agent_response(&response), structured.as_ref());
        let text = decoded.get("text").and_then(Value::as_str).unwrap_or_default().to_string();
        if text.is_empty() {
            return Err(AgentError::Failed("터미널 최종 메시지가 없습니다.".into()));
        }
        let started_at: f64 = row.try_get("started_at")?;
        let prompt: String = row.try_get("prompt")?;
        // Node `providerKind === 'local' ? providerSnapshot : null`. 정규화하지 않은 스냅숏을 그대로 쓴다.
        let provider_kind: Option<String> = row.try_get("provider_kind")?;
        let local_profile = if provider_kind.as_deref() == Some("local") {
            row.try_get::<Option<Value>, _>("provider_snapshot")?.filter(|snapshot| !snapshot.is_null())
        } else {
            None
        };
        let mut state = TurnState::new(TurnSeed {
            turn_id: turn_id.clone(),
            session_id: row.try_get("session_id")?,
            conversation_id: row.try_get("conversation_id")?,
            external_session_id: row.try_get("external_id")?,
            resume_execution_workdir: None,
            character: RuntimeCharacter {
                id: Some(row.try_get("character_id")?),
                name: row.try_get("name")?,
                seat: row.try_get("seat")?,
                backend: execution_backend.unwrap_or_default(),
                model: row.try_get("execution_model")?,
                effort: row.try_get::<Option<String>, _>("execution_effort")?.unwrap_or_default(),
                fast_mode: row.try_get::<Option<bool>, _>("execution_fast_mode")?.unwrap_or(false),
                auto_compact_percent: Some(f64::from(row.try_get::<i16, _>("auto_compact_percent")?)),
                permission: row.try_get("permission")?,
                identity_prompt: row.try_get("identity_prompt")?,
                config: row.try_get("config")?,
                local_profile,
            },
            prompt: prompt.clone(),
            execution_prompt: prompt,
            workdir: self.workdir_text().unwrap_or_default(),
            structured_result_path: None,
            attachments: Vec::new(),
            reply_recipient_ids: Vec::new(),
        });
        state.initial_generated_images = initial_generated_images;
        state.response_text = text.clone();
        state.partial_text = text.clone();
        state.usage = usage;
        state.ended_at = Some(ended_at.unwrap_or_else(now_milliseconds));
        state.usage_started_at = started_at;
        state.refresh_terminal_result = refresh_completed;
        if refresh_completed || restore_tool_activities {
            let previous = sqlx::query(
                "SELECT seq, kind, text, event_key, status, collaboration FROM turn_activities WHERE turn_id = $1::uuid ORDER BY seq",
            )
            .bind(&turn_id)
            .fetch_all(&services.pool)
            .await?;
            for entry in previous {
                let sequence: i32 = entry.try_get("seq")?;
                let kind: String = entry.try_get("kind")?;
                let entry_text: String = entry.try_get("text")?;
                let event_key: Option<String> = entry.try_get("event_key")?;
                let status: String = entry.try_get("status")?;
                let collaboration: Option<Value> = entry.try_get("collaboration")?;
                // 실시간 원본 글이 Stop보다 먼저 올 수 있다. 진행 표시용으로 잘린 긴 글까지 최종 글은 답변에만 둔다.
                let activity = json!({ "kind": kind, "text": entry_text, "eventKey": event_key, "status": status });
                if event_key.as_deref().is_some_and(|key| key.starts_with("terminal:")) && is_terminal_final_message(&activity, &text) {
                    sqlx::query("DELETE FROM turn_activities WHERE turn_id = $1::uuid AND seq = $2")
                        .bind(&turn_id)
                        .bind(sequence)
                        .execute(&services.pool)
                        .await?;
                    state.restore_activity(sequence, None, Value::Null, Value::Null, Value::Null, Value::Null);
                    continue;
                }
                state.restore_activity(
                    sequence,
                    event_key,
                    Value::String(kind),
                    Value::String(entry_text),
                    Value::String(status),
                    collaboration.unwrap_or(Value::Null),
                );
            }
        }
        if let Some(model) = resolved_model.as_deref().filter(|model| !model.is_empty()) {
            state.record_resolved_model(&services, model).await?;
        }
        if state.usage.is_none() {
            state.usage = self.terminal_turn_usage(&services, &mut state).await;
        }
        if let Some(cost) = reported_cost_usd {
            let mut usage = state.usage.take().unwrap_or_else(empty_usage);
            if let Some(fields) = usage.as_object_mut() {
                fields.insert("reportedCostUsd".into(), json!(cost));
            }
            state.usage = Some(usage);
        }
        // 터미널도 GUI와 같은 활동 표·이벤트 경로를 쓴다. 최종 답변은 따로 두고 활동을 본문에 다시 잇지 않는다.
        for activity in &activities {
            state.add_activity(&services, activity).await?;
        }
        let handle = TurnHandle::new(state);
        let mut state = handle.state.lock().await;
        self.complete(&services, &handle, &mut state, &decoded).await?;
        Ok(())
    }

    async fn add_terminal_turn_reported_cost(&self, character_id: &str, turn_id: &str, cost_usd: f64) -> Result<bool, AgentError> {
        if turn_id.is_empty() || !cost_usd.is_finite() || cost_usd <= 0.0 {
            return Ok(false);
        }
        let services = self.services()?;
        let updated: Option<String> = sqlx::query_scalar(
            "INSERT INTO usage_records (turn_id, cost_usd)
             SELECT turn.id, $3::numeric
             FROM turns AS turn
             JOIN cli_sessions AS session ON session.id = turn.cli_session_id
             WHERE turn.id = $1::uuid AND session.character_id = $2 AND turn.origin = 'terminal' AND turn.backend = 'claude'
             ON CONFLICT (turn_id) DO UPDATE
             SET cost_usd = COALESCE(usage_records.cost_usd, 0) + EXCLUDED.cost_usd
             RETURNING turn_id::text",
        )
        .bind(turn_id)
        .bind(character_id)
        .bind(number_to_string(cost_usd))
        .fetch_optional(&services.pool)
        .await?;
        if updated.is_none() {
            return Ok(false);
        }
        services.broadcast(json!({ "type": "feed.changed", "turnId": turn_id, "characterId": character_id, "costChanged": true }));
        Ok(true)
    }

    async fn interrupt_terminal_turn(&self, character_id: &str, turn_id: &str) -> Result<bool, AgentError> {
        if turn_id.is_empty() {
            return Ok(false);
        }
        let services = self.services()?;
        let updated: Option<String> = sqlx::query_scalar(
            "UPDATE turns AS turn
             SET status = 'interrupted', needs_input = false,
               error_message = '터미널 프로세스가 응답 완료 전에 종료됐습니다.',
               ended_at = now(), updated_at = now()
             FROM cli_sessions AS session
             WHERE turn.id = $1::uuid AND turn.cli_session_id = session.id AND session.character_id = $2
               AND turn.origin = 'terminal' AND turn.status = 'running'
             RETURNING turn.id::text",
        )
        .bind(turn_id)
        .bind(character_id)
        .fetch_optional(&services.pool)
        .await?;
        if updated.is_none() {
            return Ok(false);
        }
        services.broadcast(json!({ "type": "feed.changed", "turnId": turn_id, "characterId": character_id, "costChanged": true }));
        Ok(true)
    }

    async fn terminal_dispatch_busy(&self, character_id: &str) -> Result<bool, AgentError> {
        let services = self.services()?;
        let busy: Option<String> = sqlx::query_scalar(BUSY_TURN_SQL).bind(character_id).fetch_optional(&services.pool).await?;
        Ok(busy.is_some())
    }

    async fn reply_route_tags(&self, character_id: &str) -> Result<Vec<String>, AgentError> {
        let services = self.services()?;
        let tags: Vec<String> = sqlx::query_scalar(
            "SELECT recipient_character_id FROM reply_route_recipients WHERE character_id = $1 ORDER BY recipient_character_id",
        )
        .bind(character_id)
        .fetch_all(&services.pool)
        .await?;
        Ok(tags)
    }

    fn local_providers(&self) -> Option<LocalProviderService> {
        self.services().ok().and_then(|services| services.local_providers.clone())
    }
}

impl Runtime {
    /// terminalTurnUsage. Claude·Codex 터미널은 CLI가 사용량을 알려 주지 않아 디스크 기록에서 이 턴 구간만 더한다.
    /// Claude 비용은 기록에 없고 상태줄 값을 호출자가 따로 넣는다. 읽기 실패는 경고만 남기고 사용량 없이 둔다.
    async fn terminal_turn_usage(&self, services: &RuntimeServices, state: &mut TurnState) -> Option<Value> {
        let external = state.external_session_id.clone().filter(|id| !id.is_empty())?;
        let window = TurnWindow { started_at: Some(state.usage_started_at), ended_at: state.ended_at };
        match state.character.backend.as_str() {
            "codex" => codex_rollout_turn_usage(
                find_rollout_path(&services.roots.codex_sessions(), &external).as_deref(),
                window,
                CodexScope::Task,
                0,
            ),
            "claude" => {
                let projects = services.roots.claude_projects();
                let current = claude_session_path(&projects, Some(&state.workdir), &external);
                let path = if claude_session_file_matches(current.as_deref(), &external) {
                    current
                } else {
                    find_claude_session_path(&projects, &external)
                };
                let turn = claude_transcript_turn(path.as_deref(), window);
                if let Some(model) = turn.model.as_deref().filter(|model| !model.is_empty())
                    && let Err(error) = state.record_resolved_model(services, model).await
                {
                    eprintln!("터미널 턴 사용량을 읽지 못했습니다. {error}");
                    return None;
                }
                turn.usage
            }
            _ => None,
        }
    }
}
