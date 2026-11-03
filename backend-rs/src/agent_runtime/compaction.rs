// 직원 CLI 세션의 컨텍스트 압축. Node AgentRuntime의 compactionTarget·compactContext·maybeAutoCompactAfterTurn을 옮겼다.

use std::path::Path;
use std::sync::Arc;

use serde_json::{Value, json};
use sqlx::Row;

use super::claude_session::prepare_claude_session_resume;
use super::launch::{RuntimeCharacter, execution_environment, normalize_auto_compact_percent};
use super::lifecycle::{ActiveSession, active_session_matches_runtime};
use super::locate_executable;
use super::service::{AgentError, RuntimeServices};
use crate::codex_context_compactor::{CodexCompactRequest, DEFAULT_TIMEOUT, compact_codex_thread};
use crate::health::Runtime;
use crate::js::{js_compare, js_trim, js_truthy};
use crate::local_embedding::process_environment;
use crate::session_context_usage::{ContextRequest, ContextUsage, TranscriptRoots, session_context_usage};

const COMPACTION_TARGET_SQL: &str = r#"SELECT
    character.id, character.name, character.seat, character.backend, character.model, character.effort,
    character.fast_mode, character.auto_compact_percent, character.permission, character.identity_prompt,
    character.config,
    session.external_id AS external_session_id,
    conversation.workdir AS conversation_workdir,
    session_scope.repository_root AS session_repository_root,
    resume_workspace.execution_workdir AS resume_execution_workdir
  FROM characters AS character
  LEFT JOIN active_cli_sessions AS active
    ON active.character_id = character.id
  LEFT JOIN cli_sessions AS session
    ON session.id = active.cli_session_id
    AND session.ended_at IS NULL
  LEFT JOIN conversations AS conversation
    ON conversation.id = session.conversation_id
  LEFT JOIN LATERAL (
    SELECT candidate.repository_root
    FROM task_workspaces AS candidate
    WHERE candidate.cli_session_id = session.id
      AND candidate.repository_root IS NOT NULL
    ORDER BY candidate.updated_at DESC, candidate.id DESC
    LIMIT 1
  ) AS session_scope ON true
  LEFT JOIN LATERAL (
    SELECT candidate.execution_workdir
    FROM turns AS completed_turn
    JOIN task_workspaces AS candidate
      ON candidate.id = completed_turn.task_workspace_id
    WHERE completed_turn.cli_session_id = session.id
      AND completed_turn.status = 'completed'
      AND candidate.execution_workdir IS NOT NULL
    ORDER BY
      completed_turn.ended_at DESC NULLS LAST,
      completed_turn.started_at DESC,
      completed_turn.id DESC
    LIMIT 1
  ) AS resume_workspace ON true
  WHERE character.id = $1
  LIMIT 1"#;

/// compactionTarget 결과.
struct CompactionTarget {
    character: RuntimeCharacter,
    external_session_id: String,
    resume_execution_workdir: Option<String>,
    workdir: String,
}

impl Runtime {
    /// compactingCharacterIDs. 실시간 연결의 ready 메시지에 싣는다.
    pub fn compacting_character_ids(&self) -> Vec<String> {
        let mut ids: Vec<String> = self.jobs().compacting.iter().cloned().collect();
        ids.sort_by(|left, right| js_compare(left, right));
        ids
    }

    /// compactContext. 응답을 기다리던 요청이 끊겨도 압축 표시가 남지 않도록 별도 작업에서 돈다.
    pub async fn compact_context(
        self: &Arc<Self>,
        character_id: &str,
        automatic: bool,
        expected_session_id: Option<&str>,
    ) -> Result<Value, AgentError> {
        let runtime = Arc::clone(self);
        let character_id = character_id.to_string();
        let expected_session_id = expected_session_id.map(str::to_string);
        tokio::spawn(async move {
            runtime.compact_context_job(&character_id, automatic, expected_session_id.as_deref()).await
        })
        .await
        .unwrap_or_else(|join_error| Err(AgentError::Failed(join_error.to_string())))
    }

    async fn compact_context_job(
        self: &Arc<Self>,
        character_id: &str,
        automatic: bool,
        expected_session_id: Option<&str>,
    ) -> Result<Value, AgentError> {
        let services = self.services()?;
        if self.is_draining() {
            return Err(AgentError::Draining("백엔드가 안전한 전환을 준비 중이라 컨텍스트를 압축하지 않습니다.".into()));
        }
        {
            let mut jobs = self.jobs();
            if jobs.running.contains_key(character_id)
                || jobs.preparing_characters.contains(character_id)
                || jobs.compacting.contains(character_id)
            {
                return Err(AgentError::Busy("이 직원의 현재 업무와 후속 처리가 끝난 뒤 압축하세요.".into()));
            }
            jobs.compacting.insert(character_id.to_string());
        }
        services.broadcast(json!({ "type": "context.compaction.started", "characterId": character_id, "automatic": automatic }));
        let result = self.run_compaction(&services, character_id, automatic, expected_session_id).await;
        self.jobs().compacting.remove(character_id);
        match result {
            Ok(payload) => {
                services.broadcast(json!({
                    "type": "context.compacted",
                    "characterId": character_id,
                    "automatic": automatic,
                    "preTokens": payload["preTokens"],
                    "postTokens": payload["postTokens"],
                    "limitTokens": payload["limitTokens"],
                }));
                Ok(payload)
            }
            Err(error) => {
                services.broadcast(json!({
                    "type": "context.compaction.failed",
                    "characterId": character_id,
                    "automatic": automatic,
                    "errorMessage": error.to_string(),
                }));
                Err(error)
            }
        }
    }

    async fn run_compaction(
        self: &Arc<Self>,
        services: &RuntimeServices,
        character_id: &str,
        automatic: bool,
        expected_session_id: Option<&str>,
    ) -> Result<Value, AgentError> {
        let target = self.compaction_target(services, character_id).await?;
        if js_truthy(target.character.config.get("localProfileId")) {
            return Err(AgentError::Failed(
                "로컬 AI는 CLI 기본 자동 압축을 사용합니다. 수동 압축은 터미널에서 /compact를 사용하세요.".into(),
            ));
        }
        if let Some(expected) = expected_session_id.filter(|expected| !expected.is_empty())
            && target.external_session_id != expected
        {
            return Err(AgentError::Busy("압축 기준을 확인한 뒤 활성 세션이 바뀌었습니다.".into()));
        }
        let session_id = target.external_session_id.as_str();
        let before = context_usage(services, &target.character, session_id);
        let (native_pre_tokens, native_post_tokens) = match target.character.backend.as_str() {
            "claude" => {
                prepare_claude_session_resume(
                    &services.roots.claude_projects(),
                    session_id,
                    Some(&target.workdir),
                    target.resume_execution_workdir.as_deref(),
                )?;
                let executable = locate_executable("claude", &target.character.config)?;
                let worker =
                    self.acquire_claude_worker(services, &target.character, &target.workdir, Some(session_id), &executable);
                let tokens = worker.compact().await?;
                (tokens.pre_tokens, tokens.post_tokens)
            }
            "codex" => {
                let executable = locate_executable("codex", &target.character.config)?;
                // Node는 업무 폴더 없이 executionEnvironment(character)를 쓴다. 개발 도구 표시 주소는 넣지 않는다.
                let env = execution_environment(&target.character, &process_environment(), None, &services.paths);
                compact_codex_thread(CodexCompactRequest {
                    executable: &executable,
                    thread_id: session_id,
                    cwd: &target.workdir,
                    env: &env,
                    timeout: DEFAULT_TIMEOUT,
                })
                .await?;
                (None, None)
            }
            _ => return Err(AgentError::Failed("Antigravity CLI는 수동 컨텍스트 압축을 지원하지 않습니다.".into())),
        };
        let after = context_usage(services, &target.character, session_id);
        let before_used = before.map(|usage| usage.used_tokens);
        let measured_post_tokens = after
            .map(|usage| usage.used_tokens)
            .filter(|used| before_used.is_none_or(|before_used| *used < before_used));
        Ok(json!({
            "ok": true,
            "automatic": automatic,
            "backend": target.character.backend,
            "sessionId": session_id,
            "preTokens": native_pre_tokens.or(before_used),
            "postTokens": native_post_tokens.or(measured_post_tokens),
            "limitTokens": after.or(before).map(|usage| usage.limit_tokens),
        }))
    }

    /// compactionTarget. 지금 실행기와 맞는 활성 세션이 있어야 하고, 마지막 작업 공간 폴더가 남아 있으면 거기서 압축한다.
    async fn compaction_target(&self, services: &RuntimeServices, character_id: &str) -> Result<CompactionTarget, AgentError> {
        let Some(row) = sqlx::query(COMPACTION_TARGET_SQL).bind(character_id).fetch_optional(&services.pool).await? else {
            return Err(AgentError::CharacterNotFound("캐릭터를 찾을 수 없습니다.".into()));
        };
        let session = ActiveSession {
            id: String::new(),
            external_session_id: row.try_get("external_session_id")?,
            conversation_id: String::new(),
            conversation_workdir: row.try_get("conversation_workdir")?,
            session_repository_root: row.try_get("session_repository_root")?,
            workspace_repository_root: None,
            resume_execution_workdir: row.try_get("resume_execution_workdir")?,
        };
        let workdir = self.workdir_text();
        let repository_root = self.repository_root.as_deref().and_then(Path::to_str);
        let external_session_id = match session.external_session_id.clone() {
            Some(id) if !id.is_empty() && active_session_matches_runtime(&session, workdir.as_deref(), repository_root) => id,
            _ => return Err(AgentError::SessionNotFound("압축할 활성 CLI 세션이 없습니다.".into())),
        };
        let previous_workdir = js_trim(session.resume_execution_workdir.as_deref().unwrap_or("")).to_string();
        let workdir = if !previous_workdir.is_empty() && Path::new(&previous_workdir).exists() {
            previous_workdir.clone()
        } else {
            workdir.unwrap_or_default()
        };
        let config = row.try_get::<Option<Value>, _>("config")?.unwrap_or_else(|| json!({}));
        let maximum = if js_truthy(config.get("localProfileId")) { 100.0 } else { 95.0 };
        let percent: i16 = row.try_get("auto_compact_percent")?;
        let character = RuntimeCharacter {
            id: Some(row.try_get("id")?),
            name: row.try_get("name")?,
            seat: row.try_get("seat")?,
            backend: row.try_get("backend")?,
            model: row.try_get("model")?,
            effort: row.try_get("effort")?,
            fast_mode: row.try_get("fast_mode")?,
            auto_compact_percent: Some(normalize_auto_compact_percent(Some(f64::from(percent)), maximum) as f64),
            permission: row.try_get("permission")?,
            identity_prompt: row.try_get("identity_prompt")?,
            config,
            // Node 압축 대상도 resolveLocalCharacter를 거치지 않는다. 로컬 직원은 config.localProfileId로 먼저 거절한다.
            local_profile: None,
        };
        Ok(CompactionTarget {
            character,
            external_session_id,
            resume_execution_workdir: (!previous_workdir.is_empty()).then_some(previous_workdir),
            workdir,
        })
    }

    /// maybeAutoCompactAfterTurn. Claude만 직원별 기준을 넘으면 자동 압축한다. Codex는 CLI 자체 자동 압축을 쓴다.
    /// 실패는 업무 결과를 바꾸지 않고 경고로만 남긴다.
    pub(super) async fn maybe_auto_compact_after_turn(
        self: &Arc<Self>,
        services: &RuntimeServices,
        character: &RuntimeCharacter,
        external_session_id: Option<&str>,
    ) {
        // Node처럼 이 턴을 로컬 정의로 실행했는지(character.localProfile)로 거른다.
        if character.local_profile.is_some() || character.backend != "claude" {
            return;
        }
        let session_id = js_trim(external_session_id.unwrap_or(""));
        if session_id.is_empty() || self.is_draining() {
            return;
        }
        let Some(usage) = context_usage(services, character, session_id) else {
            return;
        };
        if usage.limit_tokens == 0.0 || usage.limit_tokens.is_nan() {
            return;
        }
        let threshold = normalize_auto_compact_percent(character.auto_compact_percent, 95.0) as f64;
        if usage.used_tokens * 100.0 < usage.limit_tokens * threshold {
            return;
        }
        let character_id = character.id.as_deref().unwrap_or("");
        if let Err(error) = self.compact_context_job(character_id, true, Some(session_id)).await {
            eprintln!("{} 자동 컨텍스트 압축에 실패했습니다. {error}", character.name);
        }
    }
}

/// contextUsage. 지금 시점의 세션 기록으로 컨텍스트 점유를 읽는다.
fn context_usage(services: &RuntimeServices, character: &RuntimeCharacter, session_id: &str) -> Option<ContextUsage> {
    let roots = TranscriptRoots {
        claude: services.roots.claude_projects(),
        codex: services.roots.codex_sessions(),
        antigravity: services.roots.antigravity_conversations(),
    };
    session_context_usage(
        &ContextRequest {
            backend: Some(&character.backend),
            session_id: Some(session_id),
            model: character.model.as_deref(),
            at: None,
            context_window_override: None,
        },
        &roots,
    )
}
