// 직원 CLI 실행기의 공용 부품. 실행기에 붙이는 서비스(DB·방송·경로), 실행 중 잡 표, 접수 오류를 둔다.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;
use sqlx::PgPool;

use super::launch::LaunchPaths;
use super::turn::TurnHandle;
use crate::claude_persistent_worker::{ClaudePersistentWorker, DEFAULT_INTERRUPT_RESULT_TIMEOUT, DEFAULT_SUGGESTION_GRACE};
use crate::git_workspace::{GitErrorCode, GitWorkspaceError, GitWorkspaceManager};
use crate::js::join_path;
use crate::local_embedding::LocalEmbeddingService;
use crate::local_provider::LocalResolveError;
use crate::local_provider::service::LocalProviderService;
use crate::pricing_catalog::PricingCatalogs;

/// Node broadcast. 운영에서는 웹소켓 방송이고 시험은 받은 사건을 모은다.
pub type Broadcast = Arc<dyn Fn(Value) + Send + Sync>;

/// Node homedir() 아래 CLI 기록 폴더들. 시험은 임시 폴더를 넣는다.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionRoots {
    pub home: String,
}

impl SessionRoots {
    pub fn codex_sessions(&self) -> String {
        join_path(&[&self.home, ".codex", "sessions"])
    }

    pub fn claude_projects(&self) -> String {
        join_path(&[&self.home, ".claude", "projects"])
    }

    pub fn antigravity_conversations(&self) -> String {
        join_path(&[&self.home, ".gemini", "antigravity-cli", "conversations"])
    }

    /// generatedImageRoot.
    pub fn generated_images(&self, backend: &str) -> String {
        if backend == "antigravity" {
            join_path(&[&self.home, ".gemini", "antigravity-cli", "brain"])
        } else {
            join_path(&[&self.home, ".codex", "generated_images"])
        }
    }
}

/// 실행기가 일하려면 붙어 있어야 하는 서비스. 붙기 전에는 잡 경로가 503이다.
pub struct RuntimeServices {
    pub pool: PgPool,
    pub broadcast: Broadcast,
    pub paths: LaunchPaths,
    pub roots: SessionRoots,
    pub pricing: Arc<PricingCatalogs>,
    /// Node embeddingService. 작업 기록 색인(R14)이 쓴다.
    pub embedding: Option<LocalEmbeddingService>,
    /// Claude 지속 세션의 추천 문장 유예와 중단 결과 대기. 시험만 줄인다.
    pub claude_suggestion_grace: Duration,
    pub claude_interrupt_result_timeout: Duration,
    /// Node workspaceManager. 운영은 늘 붙이고, 없으면 작업 공간 검토가 404이고 CLI 전환이 막힌다.
    pub workspace_manager: Option<Arc<GitWorkspaceManager>>,
    /// Node localProviders. 운영은 늘 붙이고, 없으면 로컬 프로필 직원의 실행과 모델 제어가 막힌다.
    pub local_providers: Option<LocalProviderService>,
}

impl RuntimeServices {
    pub fn new(pool: PgPool, broadcast: Broadcast, paths: LaunchPaths, roots: SessionRoots) -> Self {
        Self {
            pool,
            broadcast,
            paths,
            roots,
            pricing: PricingCatalogs::shared(),
            embedding: None,
            claude_suggestion_grace: DEFAULT_SUGGESTION_GRACE,
            claude_interrupt_result_timeout: DEFAULT_INTERRUPT_RESULT_TIMEOUT,
            workspace_manager: None,
            local_providers: None,
        }
    }

    pub fn broadcast(&self, event: Value) {
        (self.broadcast)(event);
    }
}

/// Node AgentRuntime의 running·claudeWorkers·compactingCharacters·preparingCharacters·preparingJobs·postProcessingJobs.
#[derive(Default)]
pub struct JobTable {
    pub(crate) running: HashMap<String, Arc<TurnHandle>>,
    pub(crate) claude_workers: HashMap<String, Arc<ClaudePersistentWorker>>,
    pub(crate) compacting: BTreeSet<String>,
    pub(crate) preparing_characters: HashSet<String>,
    pub(crate) preparing_jobs: usize,
    pub(crate) post_processing_jobs: usize,
}

impl JobTable {
    /// activeWorkCount의 잡 표 몫. 열린·여는 중인 터미널 세션 수는 Runtime::maintenance_status가 더한다.
    pub fn active_work_count(&self) -> usize {
        self.preparing_jobs + self.running.len() + self.compacting.len() + self.post_processing_jobs
    }
}

impl fmt::Debug for JobTable {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("JobTable")
            .field("running", &self.running.keys().collect::<Vec<_>>())
            .field("claude_workers", &self.claude_workers.keys().collect::<Vec<_>>())
            .field("compacting", &self.compacting)
            .field("preparing_characters", &self.preparing_characters)
            .field("preparing_jobs", &self.preparing_jobs)
            .field("post_processing_jobs", &self.post_processing_jobs)
            .finish()
    }
}

/// 잡 접수·중단 실패. Node의 AgentDrainingError·AgentBusyError·CharacterNotFoundError·AgentJobNotFoundError와
/// 그 밖의 Error(500)를 나눈다.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentError {
    Unavailable,
    Draining(String),
    Busy(String),
    /// 터미널 전달이 이미 PTY에 쓰였을 수 있는 바쁨(Node AgentBusyError + deliveryMayHaveStarted).
    /// API 응답은 Busy와 같은 409이고, 답변 자동 전달만 다시 보내지 않도록 uncertain으로 남긴다.
    DeliveryUncertain(String),
    CharacterNotFound(String),
    JobNotFound(String),
    /// AgentSessionNotFoundError. 압축할 활성 세션이 없다(404).
    SessionNotFound(String),
    /// GitWorkspaceError(409). code는 승인 실패 기록이 보고 나눈다.
    Git(GitErrorCode, String),
    Failed(String),
}

pub const RUNTIME_UNAVAILABLE: &str = "CLI 실행기가 준비되지 않았습니다.";

impl fmt::Display for AgentError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unavailable => formatter.write_str(RUNTIME_UNAVAILABLE),
            Self::Draining(message)
            | Self::Busy(message)
            | Self::DeliveryUncertain(message)
            | Self::CharacterNotFound(message)
            | Self::JobNotFound(message)
            | Self::SessionNotFound(message)
            | Self::Git(_, message)
            | Self::Failed(message) => formatter.write_str(message),
        }
    }
}

/// code 없는 작업 공간 오류는 Node가 그대로 올리는 fs 오류라 500이다.
impl From<GitWorkspaceError> for AgentError {
    fn from(error: GitWorkspaceError) -> Self {
        match error.code {
            Some(code) => Self::Git(code, error.message),
            None => Self::Failed(error.message),
        }
    }
}

impl From<sqlx::Error> for AgentError {
    fn from(error: sqlx::Error) -> Self {
        Self::Failed(crate::rag::database_message(&error))
    }
}

impl From<String> for AgentError {
    fn from(message: String) -> Self {
        Self::Failed(message)
    }
}

/// resolveLocalCharacter·validateLocalResume는 일반 Error라 500이다.
impl From<LocalResolveError> for AgentError {
    fn from(error: LocalResolveError) -> Self {
        match error {
            LocalResolveError::Invalid(message) => Self::Failed(message),
            LocalResolveError::Database(error) => Self::from(error),
        }
    }
}
