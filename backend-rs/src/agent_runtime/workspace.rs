// 작업 공간 검토·승인·거절과 작업 기록 검토 상태 동기화. Node agent-runtime.mjs의 해당 부분을 옮겼다.
// 승인·거절은 응답을 기다리던 요청이 끊겨도 Git·DB 작업이 멈추지 않도록 별도 작업에서 돌고, 그동안 후처리 수에 든다.

use std::sync::Arc;

use serde_json::{Value, json};
use sqlx::postgres::PgRow;
use sqlx::{AssertSqlSafe, PgConnection, Row};

use super::service::{AgentError, RuntimeServices};
use crate::db::with_transaction;
use crate::git_workspace::{DiffOutput, GitErrorCode, GitWorkspaceManager, TaskWorkspace};
use crate::health::Runtime;
use crate::js::{iso_string, js_string_or_empty, js_trim, now_milliseconds, utf16_prefix};
use crate::rag::{PendingFilter, RagDatabase, database_message, embed_rag_documents_best_effort};
use crate::turn_feed::iso;
use crate::work_record_memory::{ReviewTransition, sync_work_record_rag_documents, transition_turn_work_record_review};

/// MAX_WORKSPACE_DIFF_BYTES.
const MAX_WORKSPACE_DIFF_BYTES: usize = 512 * 1024;
const REVIEWABLE_STATUSES: [&str; 2] = ["awaiting_approval", "conflict"];

/// Node가 workspace.*로 읽는 열. uuid는 문자열로, timestamptz는 node-pg Date의 toISOString 꼴로 읽는다.
pub(crate) fn workspace_columns(alias: &str) -> String {
    let mut columns = vec![format!("{alias}.id::text AS id"), format!("{alias}.cli_session_id::text AS cli_session_id")];
    for column in [
        "status",
        "repository_root",
        "source_workdir",
        "worktree_path",
        "execution_workdir",
        "branch_name",
        "base_branch",
        "base_commit",
    ] {
        columns.push(format!("{alias}.{column}"));
    }
    columns.push(format!("{alias}.review_turn_id::text AS review_turn_id"));
    for column in ["review_tree", "head_commit", "changed_files", "task_commit", "merged_commit", "error_message"] {
        columns.push(format!("{alias}.{column}"));
    }
    for column in ["created_at", "updated_at", "review_requested_at", "merged_at", "rejected_at"] {
        columns.push(format!("{} AS {column}", iso(&format!("{alias}.{column}"))));
    }
    columns.join(", ")
}

/// workspaceFromRow. changed_files가 null이면 빈 목록이다.
pub(crate) fn workspace_from_row(row: &PgRow) -> Result<TaskWorkspace, sqlx::Error> {
    let changed_files: Option<Value> = row.try_get("changed_files")?;
    Ok(TaskWorkspace {
        id: row.try_get("id")?,
        cli_session_id: row.try_get("cli_session_id")?,
        status: row.try_get("status")?,
        repository_root: row.try_get("repository_root")?,
        source_workdir: row.try_get("source_workdir")?,
        worktree_path: row.try_get("worktree_path")?,
        execution_workdir: row.try_get("execution_workdir")?,
        branch_name: row.try_get("branch_name")?,
        base_branch: row.try_get("base_branch")?,
        base_commit: row.try_get("base_commit")?,
        review_turn_id: row.try_get("review_turn_id")?,
        review_tree: row.try_get("review_tree")?,
        head_commit: row.try_get("head_commit")?,
        changed_files: match changed_files {
            None | Some(Value::Null) => json!([]),
            Some(files) => files,
        },
        task_commit: row.try_get("task_commit")?,
        merged_commit: row.try_get("merged_commit")?,
        error_message: row.try_get("error_message")?,
        created_at: row.try_get("created_at")?,
        updated_at: row.try_get("updated_at")?,
        review_requested_at: row.try_get("review_requested_at")?,
        merged_at: row.try_get("merged_at")?,
        rejected_at: row.try_get("rejected_at")?,
    })
}

/// workspaceReviewRecord 결과. Node의 conversationID는 쓰는 곳이 없어 읽지 않는다.
struct ReviewRecord {
    workspace: TaskWorkspace,
    character_id: String,
    character_name: String,
}

/// workspaceReviewRecord. 검토 턴으로 작업 공간과 직원을 찾고, lock이면 작업 공간·세션 행을 잠근다.
async fn workspace_review_record(connection: &mut PgConnection, turn_id: &str, lock: bool) -> Result<ReviewRecord, AgentError> {
    let sql = format!(
        "SELECT {}, session.character_id, character.name AS character_name
         FROM task_workspaces AS workspace
         JOIN cli_sessions AS session ON session.id = workspace.cli_session_id
         JOIN characters AS character ON character.id = session.character_id
         WHERE workspace.review_turn_id = $1::uuid
         {}",
        workspace_columns("workspace"),
        if lock { "FOR UPDATE OF workspace, session" } else { "" },
    );
    let row = sqlx::query(AssertSqlSafe(sql)).bind(turn_id).fetch_optional(&mut *connection).await?;
    let Some(row) = row else {
        return Err(AgentError::JobNotFound("검토할 작업 공간을 찾을 수 없습니다.".into()));
    };
    Ok(ReviewRecord {
        workspace: workspace_from_row(&row)?,
        character_id: row.try_get("character_id")?,
        character_name: row.try_get("character_name")?,
    })
}

/// workspaceReviewPayload. 통합·종료된 공간은 원본 작업 폴더를 실행 폴더로 보인다.
fn review_payload(workspace: &TaskWorkspace, diff: &DiffOutput) -> Value {
    let execution_workdir = if matches!(workspace.status.as_str(), "merged" | "closed") {
        &workspace.source_workdir
    } else {
        &workspace.execution_workdir
    };
    json!({
        "status": workspace.status,
        "repositoryRoot": workspace.repository_root,
        "worktreePath": workspace.worktree_path,
        "executionWorkdir": execution_workdir,
        "branchName": workspace.branch_name,
        "baseBranch": workspace.base_branch,
        "baseCommit": workspace.base_commit,
        "reviewTurnId": workspace.review_turn_id,
        "reviewTree": workspace.review_tree,
        "headCommit": workspace.head_commit,
        "changedFiles": workspace.changed_files,
        "taskCommit": workspace.task_commit,
        "mergedCommit": workspace.merged_commit,
        "errorMessage": workspace.error_message,
        "reviewRequestedAt": workspace.review_requested_at,
        "mergedAt": workspace.merged_at,
        "rejectedAt": workspace.rejected_at,
        "diff": diff.diff,
        "diffTruncated": diff.diff_truncated,
    })
}

fn empty_diff() -> DiffOutput {
    DiffOutput { diff: String::new(), diff_truncated: false }
}

/// workspaceCommitMessage.
fn workspace_commit_message(character_name: &str, turn_id: &str) -> String {
    format!("OFFICESTRA: {character_name} 업무 {}", utf16_prefix(turn_id, 8))
}

fn workspace_changed(turn_id: &str, character_id: &str, status: &str) -> Value {
    json!({ "type": "workspace.changed", "turnId": turn_id, "characterId": character_id, "status": status })
}

/// withPostProcessing. 승인·거절이 도는 동안 드레인이 끝나지 않도록 후처리 수에 넣는다.
struct PostProcessing(Arc<Runtime>);

impl PostProcessing {
    fn enter(runtime: &Arc<Runtime>) -> Self {
        runtime.jobs().post_processing_jobs += 1;
        Self(Arc::clone(runtime))
    }
}

impl Drop for PostProcessing {
    fn drop(&mut self) {
        let mut jobs = self.0.jobs();
        jobs.post_processing_jobs = jobs.post_processing_jobs.saturating_sub(1);
    }
}

/// syncWorkRecordRAGBestEffort. 기록 ID가 있으면 RAG 문서를 맞추고 임베딩을 시도한다. 실패하면 경고 문구를 돌려준다.
pub(crate) async fn sync_work_record_rag_best_effort(services: &RuntimeServices, work_record_id: Option<&str>) -> Option<String> {
    let work_record_id = work_record_id.filter(|id| !id.is_empty())?;
    let synced = with_transaction(&services.pool, async |connection| {
        sync_work_record_rag_documents(connection, None, Some(work_record_id)).await
    })
    .await;
    if let Err(error) = synced {
        let message = database_message(&error);
        eprintln!("파생 RAG 동기화에 실패했습니다. 다음 기동 때 다시 색인합니다. {message}");
        return Some(message);
    }
    let service = services.embedding.as_ref()?;
    let ids = [work_record_id.to_string()];
    let filter = PendingFilter { work_record_ids: Some(&ids), ..PendingFilter::default() };
    let message = match embed_rag_documents_best_effort(&mut RagDatabase::Pool(&services.pool), service, filter).await {
        Ok(batch) => batch.error?,
        Err(error) => database_message(&error),
    };
    eprintln!("임베딩 비활성(사유): {message}. 작업 기록은 저장했고 로컬 임베딩은 나중에 백필합니다.");
    Some(message)
}

/// transitionWorkRecordReviewBestEffort. 실패해도 작업 공간 처리는 이어 가고 경고 문구만 돌려준다.
pub(crate) async fn transition_work_record_review_best_effort(
    services: &RuntimeServices,
    transition: ReviewTransition,
) -> Option<String> {
    let transitioned = with_transaction(&services.pool, async |connection| {
        transition_turn_work_record_review(connection, &transition).await
    })
    .await;
    match transitioned {
        Ok(work_record_id) => sync_work_record_rag_best_effort(services, work_record_id.as_deref()).await,
        Err(error) => {
            let message = database_message(&error);
            eprintln!("작업 기록의 검토 상태 동기화에 실패했습니다. {message}");
            Some(message)
        }
    }
}

impl Runtime {
    /// fetchWorkspaceReview. 승인 대기 공간은 지금 변경으로 검토 버전을 새로 맞춘 뒤 diff를 붙인다.
    pub async fn fetch_workspace_review(&self, turn_id: &str) -> Result<Value, AgentError> {
        let services = self.services()?;
        let Some(manager) = services.workspace_manager.clone() else {
            return Err(AgentError::JobNotFound("검토할 작업 공간을 찾을 수 없습니다.".into()));
        };
        let mut record = {
            let mut connection = services.pool.acquire().await?;
            workspace_review_record(&mut connection, turn_id, false).await?
        };
        if record.workspace.status == "awaiting_approval" {
            let current = manager.prepare_review(&record.workspace).await?;
            if record.workspace.review_tree.as_deref() != Some(current.review_tree.as_str()) {
                let changed_files = Value::Array(current.changed_files.clone());
                let columns = workspace_columns("task_workspaces");
                let refreshed = if current.has_changes {
                    sqlx::query(AssertSqlSafe(format!(
                        "UPDATE task_workspaces
                         SET review_tree = $2, head_commit = $3, changed_files = $4::jsonb, error_message = NULL,
                             review_requested_at = now(), updated_at = now()
                         WHERE id = $1::uuid AND status = 'awaiting_approval' AND review_tree = $5
                         RETURNING {columns}"
                    )))
                    .bind(&record.workspace.id)
                    .bind(&current.review_tree)
                    .bind(&current.head_commit)
                    .bind(changed_files.to_string())
                    .bind(&record.workspace.review_tree)
                    .fetch_optional(&services.pool)
                    .await?
                } else {
                    sqlx::query(AssertSqlSafe(format!(
                        "UPDATE task_workspaces
                         SET status = 'active', review_turn_id = NULL, review_tree = NULL, head_commit = $2,
                             changed_files = '[]'::jsonb, error_message = NULL, review_requested_at = NULL, updated_at = now()
                         WHERE id = $1::uuid AND status = 'awaiting_approval' AND review_tree = $3
                         RETURNING {columns}"
                    )))
                    .bind(&record.workspace.id)
                    .bind(&current.head_commit)
                    .bind(&record.workspace.review_tree)
                    .fetch_optional(&services.pool)
                    .await?
                };
                let Some(row) = refreshed else {
                    return Err(AgentError::Busy("검토 상태가 바뀌었습니다. 다시 불러오세요.".into()));
                };
                record.workspace = workspace_from_row(&row)?;
                transition_work_record_review_best_effort(
                    &services,
                    ReviewTransition {
                        turn_id: turn_id.to_string(),
                        status: if current.has_changes { "awaiting_approval" } else { "not_required" }.into(),
                        review_tree: Some(current.review_tree.clone()),
                        head_commit: Some(current.head_commit.clone()),
                        changed_files: Some(changed_files),
                        actor_type: Some("system".into()),
                        ..ReviewTransition::default()
                    },
                )
                .await;
                services.broadcast(workspace_changed(turn_id, &record.character_id, &record.workspace.status));
            }
        }
        let diff = manager.diff(&record.workspace, MAX_WORKSPACE_DIFF_BYTES).await?;
        Ok(json!({ "workspace": review_payload(&record.workspace, &diff) }))
    }

    /// approveWorkspace. 검토한 버전과 지금 버전이 같을 때만 원본 브랜치에 병합한다. 시스템 승인만 드레인 중에도 받는다.
    pub async fn approve_workspace(
        self: &Arc<Self>,
        turn_id: &str,
        expected_review_tree: Option<Value>,
        actor_type: &str,
    ) -> Result<Value, AgentError> {
        if actor_type != "system" && self.is_draining() {
            return Err(AgentError::Draining("백엔드가 안전한 전환을 준비 중이라 새 통합을 받지 않습니다.".into()));
        }
        let guard = PostProcessing::enter(self);
        let runtime = Arc::clone(self);
        let turn_id = turn_id.to_string();
        let actor_type = actor_type.to_string();
        tokio::spawn(async move {
            let _guard = guard;
            runtime.approve_workspace_accepted(&turn_id, expected_review_tree.as_ref(), &actor_type).await
        })
        .await
        .unwrap_or_else(|join_error| Err(AgentError::Failed(join_error.to_string())))
    }

    async fn approve_workspace_accepted(
        &self,
        turn_id: &str,
        expected_review_tree: Option<&Value>,
        actor_type: &str,
    ) -> Result<Value, AgentError> {
        let services = self.services()?;
        let Some(manager) = services.workspace_manager.clone() else {
            return Err(AgentError::JobNotFound("통합할 작업 공간을 찾을 수 없습니다.".into()));
        };
        let reviewed_tree = js_trim(&js_string_or_empty(expected_review_tree)).to_string();
        if reviewed_tree.is_empty() {
            return Err(AgentError::Busy("검토한 변경 버전을 확인할 수 없습니다. diff를 다시 불러오세요.".into()));
        }
        let merged = with_transaction(&services.pool, async |connection| {
            let record = workspace_review_record(connection, turn_id, true).await?;
            if !REVIEWABLE_STATUSES.contains(&record.workspace.status.as_str()) {
                return Err(AgentError::Busy("현재 상태에서는 이 변경사항을 승인할 수 없습니다.".into()));
            }
            if record.workspace.review_tree.as_deref() != Some(reviewed_tree.as_str()) {
                return Err(AgentError::Busy("검토 뒤 변경사항이 갱신됐습니다. diff를 다시 확인하세요.".into()));
            }
            sqlx::query("SELECT pg_advisory_xact_lock(hashtext($1))")
                .bind(&record.workspace.repository_root)
                .execute(&mut *connection)
                .await?;
            let transitioned: Option<String> = sqlx::query_scalar(
                "UPDATE task_workspaces SET status = 'merging', error_message = NULL, updated_at = now()
                 WHERE id = $1::uuid AND status IN ('awaiting_approval', 'conflict')
                 RETURNING id::text",
            )
            .bind(&record.workspace.id)
            .fetch_optional(&mut *connection)
            .await?;
            if transitioned.is_none() {
                return Err(AgentError::Busy("다른 통합 작업이 이미 이 변경사항을 처리하고 있습니다.".into()));
            }
            let commit_message = workspace_commit_message(&record.character_name, turn_id);
            let approved = manager.approve(&record.workspace, &reviewed_tree, Some(&commit_message)).await?;
            sqlx::query(
                "UPDATE task_workspaces
                 SET status = 'merged', task_commit = $2, merged_commit = $3, error_message = NULL,
                     merged_at = now(), updated_at = now()
                 WHERE id = $1::uuid",
            )
            .bind(&record.workspace.id)
            .bind(&approved.task_commit)
            .bind(&approved.merged_commit)
            .execute(&mut *connection)
            .await?;
            let mut workspace = record.workspace;
            workspace.status = "merged".into();
            workspace.task_commit = Some(approved.task_commit);
            workspace.merged_commit = Some(approved.merged_commit);
            workspace.error_message = None;
            workspace.merged_at = Some(iso_string(now_milliseconds()));
            Ok((record.character_id, workspace))
        })
        .await;
        let (character_id, mut workspace) = match merged {
            Ok(merged) => merged,
            Err(error) => {
                // 원래 Git 안전 오류를 호출자에게 그대로 전달한다.
                let _ = self.record_workspace_approval_error(&services, &manager, turn_id, &error).await;
                return Err(error);
            }
        };
        transition_work_record_review_best_effort(
            &services,
            ReviewTransition {
                turn_id: turn_id.to_string(),
                status: "merged".into(),
                task_commit: workspace.task_commit.clone(),
                merged_commit: workspace.merged_commit.clone(),
                review_tree: Some(reviewed_tree.clone()),
                head_commit: workspace.head_commit.clone(),
                changed_files: Some(workspace.changed_files.clone()),
                error_message: None,
                actor_type: Some(if actor_type == "system" { "system" } else { "user" }.into()),
            },
        )
        .await;
        let mut cleanup_warning = None;
        if let Err(error) = manager.cleanup(&workspace).await {
            let warning = format!("통합은 완료됐지만 작업 공간 정리에 실패했습니다. {}", error.message);
            sqlx::query(
                "UPDATE task_workspaces SET error_message = $2, updated_at = now() WHERE id = $1::uuid AND status = 'merged'",
            )
            .bind(&workspace.id)
            .bind(&warning)
            .execute(&services.pool)
            .await?;
            workspace.error_message = Some(warning.clone());
            cleanup_warning = Some(warning);
        }
        services.broadcast(workspace_changed(turn_id, &character_id, "merged"));
        Ok(json!({
            "workspace": review_payload(&workspace, &empty_diff()),
            "cleanupWarning": cleanup_warning,
        }))
    }

    /// recordWorkspaceApprovalError. code가 있는 오류(Git 안전 오류·DB·파일 오류)만 작업 공간에 남긴다.
    /// 검토 뒤 바뀐 경우는 지금 변경으로 검토를 다시 맞추고, 충돌은 충돌 상태로 둔다.
    async fn record_workspace_approval_error(
        &self,
        services: &RuntimeServices,
        manager: &GitWorkspaceManager,
        turn_id: &str,
        error: &AgentError,
    ) -> Result<(), AgentError> {
        let code = match error {
            AgentError::Git(code, _) => Some(*code),
            AgentError::Failed(_) => None,
            _ => return Ok(()),
        };
        let message = error.to_string();
        if code == Some(GitErrorCode::ChangedAfterReview) {
            let record = {
                let mut connection = services.pool.acquire().await?;
                workspace_review_record(&mut connection, turn_id, false).await?
            };
            let review = manager.prepare_review(&record.workspace).await?;
            let changed_files = Value::Array(review.changed_files.clone());
            let updated = sqlx::query(
                "UPDATE task_workspaces
                 SET status = CASE WHEN $7 THEN 'awaiting_approval' ELSE 'active' END,
                     review_turn_id = CASE WHEN $7 THEN $6::uuid ELSE NULL END,
                     review_tree = CASE WHEN $7 THEN $2 ELSE NULL END,
                     head_commit = $3,
                     changed_files = $4::jsonb,
                     error_message = CASE WHEN $7 THEN $5 ELSE NULL END,
                     review_requested_at = CASE WHEN $7 THEN now() ELSE NULL END,
                     updated_at = now()
                 WHERE id = $1::uuid AND review_turn_id = $6::uuid AND status IN ('awaiting_approval', 'conflict')",
            )
            .bind(&record.workspace.id)
            .bind(&review.review_tree)
            .bind(&review.head_commit)
            .bind(changed_files.to_string())
            .bind(&message)
            .bind(turn_id)
            .bind(review.has_changes)
            .execute(&services.pool)
            .await?
            .rows_affected();
            if updated > 0 {
                transition_work_record_review_best_effort(
                    services,
                    ReviewTransition {
                        turn_id: turn_id.to_string(),
                        status: if review.has_changes { "awaiting_approval" } else { "not_required" }.into(),
                        review_tree: Some(review.review_tree.clone()),
                        head_commit: Some(review.head_commit.clone()),
                        changed_files: Some(changed_files),
                        error_message: review.has_changes.then(|| message.clone()),
                        actor_type: Some("system".into()),
                        ..ReviewTransition::default()
                    },
                )
                .await;
                let status = if review.has_changes { "awaiting_approval" } else { "active" };
                services.broadcast(workspace_changed(turn_id, &record.character_id, status));
            }
            return Ok(());
        }
        let status = if code == Some(GitErrorCode::Conflict) { "conflict" } else { "awaiting_approval" };
        let rows = sqlx::query(
            "UPDATE task_workspaces AS workspace
             SET status = CASE WHEN workspace.status = 'conflict' THEN 'conflict' ELSE $2 END,
                 error_message = $3,
                 updated_at = now()
             FROM cli_sessions AS session
             WHERE workspace.review_turn_id = $1::uuid
               AND session.id = workspace.cli_session_id
               AND workspace.status IN ('awaiting_approval', 'conflict')
             RETURNING session.character_id, workspace.status",
        )
        .bind(turn_id)
        .bind(status)
        .bind(&message)
        .fetch_all(&services.pool)
        .await?;
        if let Some(row) = rows.first() {
            let character_id: String = row.try_get("character_id")?;
            let saved_status: Option<String> = row.try_get("status")?;
            let saved_status = saved_status.unwrap_or_else(|| status.to_string());
            transition_work_record_review_best_effort(
                services,
                ReviewTransition {
                    turn_id: turn_id.to_string(),
                    status: saved_status.clone(),
                    error_message: Some(message.clone()),
                    actor_type: Some("system".into()),
                    ..ReviewTransition::default()
                },
            )
            .await;
            services.broadcast(workspace_changed(turn_id, &character_id, &saved_status));
        }
        Ok(())
    }

    /// rejectWorkspace. 승인 대기·충돌 공간을 거절로 닫고 작업 기록을 보관한다. Git 정리는 하지 않는다.
    pub async fn reject_workspace(self: &Arc<Self>, turn_id: &str) -> Result<Value, AgentError> {
        if self.is_draining() {
            return Err(AgentError::Draining("백엔드가 안전한 전환을 준비 중이라 새 거절을 받지 않습니다.".into()));
        }
        let guard = PostProcessing::enter(self);
        let runtime = Arc::clone(self);
        let turn_id = turn_id.to_string();
        tokio::spawn(async move {
            let _guard = guard;
            runtime.reject_workspace_accepted(&turn_id).await
        })
        .await
        .unwrap_or_else(|join_error| Err(AgentError::Failed(join_error.to_string())))
    }

    async fn reject_workspace_accepted(&self, turn_id: &str) -> Result<Value, AgentError> {
        let services = self.services()?;
        let (character_id, work_record_id, workspace) = with_transaction(&services.pool, async |connection| {
            let record = workspace_review_record(connection, turn_id, true).await?;
            if !REVIEWABLE_STATUSES.contains(&record.workspace.status.as_str()) {
                return Err(AgentError::Busy("현재 상태에서는 이 변경사항을 거절할 수 없습니다.".into()));
            }
            let rejected: Option<String> = sqlx::query_scalar(
                "UPDATE task_workspaces SET status = 'rejected', error_message = NULL, rejected_at = now(), updated_at = now()
                 WHERE id = $1::uuid AND status IN ('awaiting_approval', 'conflict')
                 RETURNING id::text",
            )
            .bind(&record.workspace.id)
            .fetch_optional(&mut *connection)
            .await?;
            if rejected.is_none() {
                return Err(AgentError::Busy("다른 작업이 이미 이 변경사항을 처리했습니다.".into()));
            }
            let work_record_id = transition_turn_work_record_review(
                connection,
                &ReviewTransition {
                    turn_id: turn_id.to_string(),
                    status: "rejected".into(),
                    review_tree: record.workspace.review_tree.clone(),
                    head_commit: record.workspace.head_commit.clone(),
                    changed_files: Some(record.workspace.changed_files.clone()),
                    ..ReviewTransition::default()
                },
            )
            .await?;
            let mut workspace = record.workspace;
            workspace.status = "rejected".into();
            workspace.error_message = None;
            workspace.rejected_at = Some(iso_string(now_milliseconds()));
            Ok::<_, AgentError>((record.character_id, work_record_id, workspace))
        })
        .await?;
        sync_work_record_rag_best_effort(&services, work_record_id.as_deref()).await;
        services.broadcast(workspace_changed(turn_id, &character_id, "rejected"));
        Ok(json!({ "workspace": review_payload(&workspace, &empty_diff()) }))
    }
}
