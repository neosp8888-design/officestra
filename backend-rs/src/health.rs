// /health 응답과 유지보수(드레인) 상태. Node의 backend/src/health.mjs와 같은 필드·순서를 낸다.

use std::fmt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};

use serde::Serialize;

use crate::agent_runtime::service::{JobTable, RuntimeServices};
use crate::git_workspace::{TaskWorkspace, WorkspaceReview};
use crate::terminal_sessions::TerminalSessionManager;

pub const OFFICE_BACKEND_SERVICE: &str = "officestra-backend";
pub const OFFICE_BACKEND_API_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct MaintenanceStatus {
    pub accepting_jobs: bool,
    pub draining: bool,
    pub active_turn_count: usize,
    pub idle: bool,
}

/// CLI 전환 전에 끝내야 할 작업 공간 계획. agent-runtime의 inspectWorkspaceForSessionEnd 결과다.
/// 활성 작업 공간이 있으면 그 공간과 최근 업무 턴, 지금 검토 결과를 함께 담는다.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionEndPlan {
    pub character_id: String,
    pub workspace: Option<TaskWorkspace>,
    pub review_turn_id: Option<String>,
    pub review: Option<WorkspaceReview>,
}

impl SessionEndPlan {
    /// plan.review?.hasChanges.
    pub fn has_review_changes(&self) -> bool {
        self.review.as_ref().is_some_and(|review| review.has_changes)
    }
}

/// Node AgentRuntime. 드레인 플래그와 실행 중 잡 표를 가지고, 잡 실행은 서비스가 붙은 뒤에만 한다.
/// 접수·실행·중단 등은 agent_runtime/lifecycle.rs에 있다.
pub struct Runtime {
    pub workdir: Option<PathBuf>,
    pub repository_root: Option<PathBuf>,
    draining: AtomicBool,
    pub(crate) jobs: Mutex<JobTable>,
    pub(crate) services: OnceLock<Arc<RuntimeServices>>,
    /// Node setTerminalSessionRegistry. 관리자가 실행기를 쥐므로 여기서는 약한 참조로 순환을 끊는다.
    pub(crate) terminal_sessions: OnceLock<Weak<TerminalSessionManager<Runtime>>>,
}

impl fmt::Debug for Runtime {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Runtime")
            .field("workdir", &self.workdir)
            .field("repository_root", &self.repository_root)
            .field("draining", &self.draining)
            .field("jobs", &self.jobs)
            .field("services", &self.services.get().is_some())
            .field("terminal_sessions", &self.terminal_sessions.get().is_some())
            .finish()
    }
}

impl Runtime {
    pub fn new(workdir: Option<PathBuf>, repository_root: Option<PathBuf>) -> Self {
        Self {
            workdir,
            repository_root,
            draining: AtomicBool::new(false),
            jobs: Mutex::default(),
            services: OnceLock::new(),
            terminal_sessions: OnceLock::new(),
        }
    }

    /// 터미널 관리자를 한 번 붙인다. 이미 붙어 있으면 false다.
    pub fn attach_terminal_sessions(&self, manager: &Arc<TerminalSessionManager<Runtime>>) -> bool {
        self.terminal_sessions.set(Arc::downgrade(manager)).is_ok()
    }

    /// 붙은 터미널 관리자. 없거나 이미 내려갔으면 None이다(Node terminalSessions가 null인 경우).
    pub fn terminal_sessions(&self) -> Option<Arc<TerminalSessionManager<Runtime>>> {
        self.terminal_sessions.get().and_then(Weak::upgrade)
    }

    pub fn maintenance_status(&self) -> MaintenanceStatus {
        let draining = self.draining.load(Ordering::SeqCst);
        let terminal_count = self.terminal_sessions().map_or(0, |manager| manager.size());
        let active_turn_count = self.jobs().active_work_count() + terminal_count;
        MaintenanceStatus {
            accepting_jobs: !draining,
            draining,
            active_turn_count,
            idle: active_turn_count == 0,
        }
    }

    pub fn is_draining(&self) -> bool {
        self.draining.load(Ordering::SeqCst)
    }

    pub fn begin_drain(&self) -> MaintenanceStatus {
        self.draining.store(true, Ordering::SeqCst);
        self.maintenance_status()
    }

    pub fn cancel_drain(&self) -> MaintenanceStatus {
        self.draining.store(false, Ordering::SeqCst);
        self.maintenance_status()
    }
}

#[derive(Debug, Serialize)]
pub struct DatabaseStatus {
    pub ok: bool,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Health {
    pub ok: bool,
    pub service: &'static str,
    pub api_version: u32,
    pub pid: u32,
    #[serde(rename = "releaseID")]
    pub release_id: Option<String>,
    #[serde(flatten)]
    pub maintenance: MaintenanceStatus,
    pub workdir: Option<PathBuf>,
    pub repository_root: Option<PathBuf>,
    pub database: DatabaseStatus,
}

pub fn health(runtime: &Runtime, pid: u32, release_id: Option<&str>, database_ok: bool) -> Health {
    Health {
        ok: true,
        service: OFFICE_BACKEND_SERVICE,
        api_version: OFFICE_BACKEND_API_VERSION,
        pid,
        release_id: release_id.map(str::to_string),
        maintenance: runtime.maintenance_status(),
        workdir: runtime.workdir.clone(),
        repository_root: runtime.repository_root.clone(),
        database: DatabaseStatus { ok: database_ok },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn health_json은_node와_같은_키_순서다() {
        let root = PathBuf::from("/Users/neo/office");
        let runtime = Runtime::new(Some(root.clone()), Some(root));
        let json = serde_json::to_string(&health(&runtime, 42, Some("abc"), true)).unwrap();
        assert_eq!(
            json,
            r#"{"ok":true,"service":"officestra-backend","apiVersion":1,"pid":42,"releaseID":"abc","acceptingJobs":true,"draining":false,"activeTurnCount":0,"idle":true,"workdir":"/Users/neo/office","repositoryRoot":"/Users/neo/office","database":{"ok":true}}"#
        );
    }

    #[test]
    fn 드레인은_접수를_막고_취소하면_되돌린다() {
        let runtime = Runtime::new(None, None);
        let status = runtime.begin_drain();
        assert!(status.draining);
        assert!(!status.accepting_jobs);
        assert!(runtime.is_draining());
        let status = runtime.cancel_drain();
        assert!(!status.draining);
        assert!(status.accepting_jobs);
        let json = serde_json::to_string(&health(&runtime, 1, None, false)).unwrap();
        assert!(json.contains(r#""releaseID":null"#));
        assert!(json.contains(r#""workdir":null,"repositoryRoot":null,"database":{"ok":false}"#));
    }
}
