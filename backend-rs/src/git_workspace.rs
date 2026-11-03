// 업무별 Git worktree를 만들고 검토된 변경만 원본 브랜치에 병합한다. Node git-workspace.mjs를 옮겼다.

use std::collections::HashSet;
use std::fmt;
use std::os::unix::fs::MetadataExt;
use std::process::Stdio;

use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Command;
use tokio::sync::Mutex;
use unicode_normalization::UnicodeNormalization;

use crate::js::{join_path, js_trim, node_fs_message, relative_path, resolve_path, resolve_paths, uv_error};
use crate::local_artifacts::home_directory;

pub const DEFAULT_DIFF_BYTES: usize = 512 * 1024;
const GIT_OUTPUT_BYTES: usize = 8 * 1024 * 1024;
const GIT_DIFF_ERROR_BYTES: usize = 64 * 1024;
const FROZEN_WORK_RECORD_PATHS: [&str; 2] = ["checklist.md", "context-notes.md"];
const APPROVAL_HOOK_NAMES: [&str; 8] = [
    "post-index-change",
    "pre-commit",
    "pre-merge-commit",
    "prepare-commit-msg",
    "commit-msg",
    "post-commit",
    "post-merge",
    "reference-transaction",
];
const COMMAND_FAILED: &str = "Git 명령 실행에 실패했습니다.";

/// GitWorkspaceError의 code. Node ERROR_CODES와 같은 다섯 가지다.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GitErrorCode {
    NotClean,
    ChangedAfterReview,
    Conflict,
    InvalidState,
    CommandFailed,
}

impl GitErrorCode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NotClean => "not-clean",
            Self::ChangedAfterReview => "changed-after-review",
            Self::Conflict => "conflict",
            Self::InvalidState => "invalid-state",
            Self::CommandFailed => "command-failed",
        }
    }
}

/// 작업 공간 처리 실패. code가 있으면 Node GitWorkspaceError(경로에서 409)이고,
/// 없으면 Node가 그대로 올려 보내는 fs 오류(EACCES 등, 경로에서 500)다.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitWorkspaceError {
    pub code: Option<GitErrorCode>,
    pub message: String,
    /// isMissingGitExecutable. git 실행 파일이 없어 시작하지 못했다.
    missing_executable: bool,
}

impl GitWorkspaceError {
    pub fn new(code: GitErrorCode, message: impl Into<String>) -> Self {
        Self { code: Some(code), message: message.into(), missing_executable: false }
    }

    pub fn filesystem(message: impl Into<String>) -> Self {
        Self { code: None, message: message.into(), missing_executable: false }
    }

    pub fn is_git_workspace_error(&self) -> bool {
        self.code.is_some()
    }
}

impl fmt::Display for GitWorkspaceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

fn invalid(message: impl Into<String>) -> GitWorkspaceError {
    GitWorkspaceError::new(GitErrorCode::InvalidState, message)
}

/// workspaceFromRow. 계획(planProvision 결과)도 같은 모양을 쓰고, 계획에는 경로·브랜치·기준 커밋만 채운다.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TaskWorkspace {
    pub id: String,
    pub cli_session_id: String,
    pub status: String,
    pub repository_root: String,
    pub source_workdir: String,
    pub worktree_path: String,
    pub execution_workdir: String,
    pub branch_name: String,
    pub base_branch: String,
    pub base_commit: String,
    pub review_turn_id: Option<String>,
    pub review_tree: Option<String>,
    pub head_commit: Option<String>,
    pub changed_files: Value,
    pub task_commit: Option<String>,
    pub merged_commit: Option<String>,
    pub error_message: Option<String>,
    pub created_at: Option<String>,
    pub updated_at: Option<String>,
    pub review_requested_at: Option<String>,
    pub merged_at: Option<String>,
    pub rejected_at: Option<String>,
}

/// prepareReview 결과.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceReview {
    pub has_changes: bool,
    pub review_tree: String,
    pub head_commit: String,
    pub changed_files: Vec<Value>,
}

/// limitedGitOutput 결과.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffOutput {
    pub diff: String,
    pub diff_truncated: bool,
}

/// approve 결과.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApprovedMerge {
    pub task_commit: String,
    pub merged_commit: String,
}

// ---- git 실행 ----

/// gitResult 결과. exit_code가 None이면 신호로 끝났거나 출력 한도를 넘겼다.
struct GitResult {
    exit_code: Option<i32>,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

impl GitResult {
    fn stdout_text(&self) -> String {
        String::from_utf8_lossy(&self.stdout).into_owned()
    }

    fn stderr_text(&self) -> String {
        String::from_utf8_lossy(&self.stderr).into_owned()
    }
}

/// gitEnvironment와 `git -C <폴더> …`. 셸을 거치지 않는다.
fn git_command(directory: &str, arguments: &[&str]) -> Command {
    let mut command = Command::new("git");
    command
        .arg("-C")
        .arg(directory)
        .args(arguments)
        .env("LANG", "C")
        .env("LC_ALL", "C")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_EDITOR", "true")
        .env("GIT_MERGE_AUTOEDIT", "no")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    command
}

/// 처음 limit 바이트만 남기고 나머지는 읽어 버린다. 넘친 바이트가 있었는지 함께 돌려준다.
async fn read_limited(reader: Option<impl AsyncRead + Unpin>, limit: usize) -> (Vec<u8>, bool) {
    let Some(mut reader) = reader else {
        return (Vec::new(), false);
    };
    let mut kept = Vec::new();
    let mut overflowed = false;
    let mut buffer = vec![0_u8; 64 * 1024];
    loop {
        let count = match reader.read(&mut buffer).await {
            Ok(0) | Err(_) => break,
            Ok(count) => count,
        };
        let remaining = limit.saturating_sub(kept.len());
        kept.extend_from_slice(&buffer[..count.min(remaining)]);
        if count > remaining {
            overflowed = true;
        }
    }
    (kept, overflowed)
}

/// gitResult. execFile처럼 stdout·stderr 각각 8MB를 넘으면 실패로 보고, 허용한 종료 코드만 결과로 돌려준다.
async fn git_result(directory: &str, arguments: &[&str], allowed_exit_codes: &[i32]) -> Result<GitResult, GitWorkspaceError> {
    let mut child = git_command(directory, arguments).spawn().map_err(|error| GitWorkspaceError {
        code: Some(GitErrorCode::CommandFailed),
        message: COMMAND_FAILED.into(),
        missing_executable: error.kind() == std::io::ErrorKind::NotFound,
    })?;
    let ((stdout, stdout_overflowed), (stderr, stderr_overflowed)) = tokio::join!(
        read_limited(child.stdout.take(), GIT_OUTPUT_BYTES),
        read_limited(child.stderr.take(), GIT_OUTPUT_BYTES),
    );
    let status = child.wait().await.ok();
    let overflowed = stdout_overflowed || stderr_overflowed;
    let result = GitResult {
        exit_code: if overflowed { None } else { status.and_then(|status| status.code()) },
        stdout,
        stderr,
    };
    match result.exit_code {
        Some(0) => Ok(result),
        Some(code) if allowed_exit_codes.contains(&code) => Ok(result),
        _ => Err(command_error(COMMAND_FAILED, &result)),
    }
}

/// gitText. stdout을 UTF-8로 읽어 앞뒤 공백을 지운다.
async fn git_text(directory: &str, arguments: &[&str]) -> Result<String, GitWorkspaceError> {
    let result = git_result(directory, arguments, &[]).await?;
    Ok(js_trim(&result.stdout_text()).to_string())
}

/// gitBuffer의 stdout.
async fn git_buffer(directory: &str, arguments: &[&str]) -> Result<Vec<u8>, GitWorkspaceError> {
    Ok(git_result(directory, arguments, &[]).await?.stdout)
}

/// commandError. 상세는 stderr, 없으면 stdout이다.
fn command_error(message: &str, result: &GitResult) -> GitWorkspaceError {
    let raw = if result.stderr.is_empty() { result.stdout_text() } else { result.stderr_text() };
    let detail = js_trim(&raw);
    GitWorkspaceError::new(
        GitErrorCode::CommandFailed,
        if detail.is_empty() { message.to_string() } else { format!("{message} {detail}") },
    )
}

/// limitedGitOutput. diff는 max_bytes까지만 남기고 잘렸는지 알린다. 오류 출력은 64KB까지만 남긴다.
async fn limited_git_output(directory: &str, arguments: &[&str], max_bytes: usize) -> Result<DiffOutput, GitWorkspaceError> {
    let mut child = git_command(directory, arguments)
        .spawn()
        .map_err(|_| GitWorkspaceError::new(GitErrorCode::CommandFailed, "Git diff 명령을 시작하지 못했습니다."))?;
    let ((stdout, diff_truncated), (stderr, _)) = tokio::join!(
        read_limited(child.stdout.take(), max_bytes),
        read_limited(child.stderr.take(), GIT_DIFF_ERROR_BYTES),
    );
    let status = child.wait().await.ok();
    let result = GitResult { exit_code: status.and_then(|status| status.code()), stdout, stderr };
    if result.exit_code != Some(0) {
        return Err(command_error("Git diff 생성에 실패했습니다.", &result));
    }
    Ok(DiffOutput { diff: result.stdout_text(), diff_truncated })
}

// ---- 경로·출력 보조 ----

fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes).iter().map(|byte| format!("{byte:02x}")).collect()
}

/// canonicalDirectory. realpath 실패는 invalid-state다.
async fn canonical_directory(path: &str) -> Result<String, GitWorkspaceError> {
    tokio::fs::canonicalize(path)
        .await
        .map(|resolved| resolved.to_string_lossy().into_owned())
        .map_err(|_| invalid(format!("작업 폴더를 확인할 수 없습니다: {path}")))
}

/// pathExists. fs.access처럼 심볼릭 링크를 따라간다.
async fn path_exists(path: &str) -> bool {
    tokio::fs::metadata(path).await.is_ok()
}

/// path.posix.dirname. 여기서는 realpath로 정규화한 절대 경로만 받는다.
fn dirname(path: &str) -> String {
    match path.rfind('/') {
        Some(0) => "/".into(),
        Some(index) => path[..index].to_string(),
        None => ".".into(),
    }
}

fn is_outside_directory(path: &str) -> bool {
    path == ".." || path.starts_with("../") || path.starts_with("..\\")
}

/// parseNullSeparated. 끝의 빈 조각과 빈 항목을 버린다.
fn parse_null_separated(buffer: &[u8]) -> Vec<String> {
    let text = String::from_utf8_lossy(buffer);
    text.split('\0').filter(|value| !value.is_empty()).map(str::to_string).collect()
}

/// parseChangedFiles. 이름 바꿈·복사는 이전 경로와 새 경로가 이어 나온다.
fn parse_changed_files(buffer: &[u8]) -> Vec<Value> {
    let text = String::from_utf8_lossy(buffer);
    let mut values: Vec<&str> = text.split('\0').collect();
    if values.last() == Some(&"") {
        values.pop();
    }
    let mut changed_files = Vec::new();
    let mut index = 0;
    while index < values.len() {
        let status = values[index];
        index += 1;
        if status.is_empty() {
            break;
        }
        let mut next = || {
            let value = values.get(index).copied();
            index += 1;
            value
        };
        if status.starts_with('R') || status.starts_with('C') {
            let previous_path = next();
            let path = next();
            let mut entry = json!({ "status": status });
            if let Some(path) = path {
                entry["path"] = json!(path);
            }
            if let Some(previous_path) = previous_path {
                entry["previousPath"] = json!(previous_path);
            }
            changed_files.push(entry);
        } else {
            let mut entry = json!({ "status": status });
            if let Some(path) = next() {
                entry["path"] = json!(path);
            }
            changed_files.push(entry);
        }
    }
    changed_files
}

/// requireUnchangedFrozenWorkRecords.
fn require_unchanged_frozen_work_records(changed_files: &[Value]) -> Result<(), GitWorkspaceError> {
    let mut frozen: Vec<&str> = Vec::new();
    for entry in changed_files {
        for key in ["previousPath", "path"] {
            if let Some(path) = entry.get(key).and_then(Value::as_str)
                && FROZEN_WORK_RECORD_PATHS.contains(&path)
                && !frozen.contains(&path)
            {
                frozen.push(path);
            }
        }
    }
    if frozen.is_empty() {
        return Ok(());
    }
    Err(invalid(format!("v1.0 동결 작업 기록은 변경할 수 없습니다: {}", frozen.join(", "))))
}

/// validateWorkspaceShape. 일곱 경로·브랜치 값이 모두 있어야 한다.
fn validate_workspace_shape(workspace: &TaskWorkspace) -> Result<(), GitWorkspaceError> {
    let required = [
        &workspace.repository_root,
        &workspace.source_workdir,
        &workspace.worktree_path,
        &workspace.execution_workdir,
        &workspace.branch_name,
        &workspace.base_branch,
        &workspace.base_commit,
    ];
    if required.iter().any(|value| value.is_empty()) {
        return Err(invalid("Git workspace 정보가 올바르지 않습니다."));
    }
    Ok(())
}

/// safeSegment. 소문자 영숫자·점·밑줄·붙임표만 남기고 64자로 자른다.
fn safe_segment(value: &str) -> String {
    let lowered = js_trim(value).to_lowercase();
    let mut replaced = String::with_capacity(lowered.len());
    let mut in_run = false;
    for character in lowered.chars() {
        if character.is_ascii_lowercase() || character.is_ascii_digit() || matches!(character, '.' | '_' | '-') {
            replaced.push(character);
            in_run = false;
        } else if !in_run {
            replaced.push('-');
            in_run = true;
        }
    }
    let trimmed = replaced.trim_matches(|character| character == '-' || character == '.');
    let normalized: String = trimmed.chars().take(64).collect();
    if normalized.is_empty() { "workspace".into() } else { normalized }
}

/// workspaceBranchName.
fn workspace_branch_name(character_id: &str, workspace_id: &str) -> String {
    let character = safe_segment(character_id);
    let workspace: String = safe_segment(workspace_id).chars().take(48).collect();
    let suffix = &sha256_hex(format!("{character_id}\0{workspace_id}").as_bytes())[..8];
    format!("officestra/{character}/{workspace}-{suffix}")
}

fn repository_key(repository_root: &str) -> String {
    sha256_hex(repository_root.as_bytes())[..12].to_string()
}

// ---- 저장소 확인 ----

/// canonicalProjectRoot. 작업 폴더의 실제 경로를 구하고, Git 저장소면 최상위 폴더를 돌려준다.
pub async fn canonical_project_root(source_workdir: &str) -> Result<String, GitWorkspaceError> {
    let canonical = canonical_directory(&resolve_path(source_workdir)).await?;
    Ok(repository_root_for(&canonical).await?.unwrap_or(canonical))
}

/// repositoryRootFor. Git 저장소가 아니라고 확실할 때만 None이다.
async fn repository_root_for(source_workdir: &str) -> Result<Option<String>, GitWorkspaceError> {
    let probe = match git_result(source_workdir, &["rev-parse", "--show-toplevel"], &[128]).await {
        Ok(probe) => probe,
        Err(error) if error.missing_executable && !has_git_marker(source_workdir).await => return Ok(None),
        Err(error) => return Err(error),
    };
    if probe.exit_code == Some(0) {
        return Ok(Some(canonical_directory(js_trim(&probe.stdout_text())).await?));
    }
    if is_explicit_non_repository(&probe) && !has_git_marker(source_workdir).await {
        return Ok(None);
    }
    Err(command_error("Git 저장소 확인에 실패했습니다.", &probe))
}

fn is_explicit_non_repository(result: &GitResult) -> bool {
    result.exit_code == Some(128)
        && result.stderr_text().lines().any(|line| {
            line.strip_prefix("fatal: not a git repository").is_some_and(|rest| {
                rest.starts_with(':') || rest.starts_with(" (or any of the parent directories):")
            })
        })
}

async fn has_git_marker(start_directory: &str) -> bool {
    let mut directory = start_directory.to_string();
    loop {
        if path_exists(&join_path(&[&directory, ".git"])).await {
            return true;
        }
        let parent = dirname(&directory);
        if parent == directory {
            return false;
        }
        directory = parent;
    }
}

async fn current_branch(directory: &str) -> Result<String, GitWorkspaceError> {
    let result = git_result(directory, &["symbolic-ref", "--quiet", "--short", "HEAD"], &[1, 128]).await?;
    let branch = js_trim(&result.stdout_text()).to_string();
    if result.exit_code != Some(0) || branch.is_empty() {
        return Err(invalid("분리된 HEAD에서는 자동 worktree 병합을 사용할 수 없습니다."));
    }
    Ok(branch)
}

async fn branch_exists(repository_root: &str, branch_name: &str) -> Result<bool, GitWorkspaceError> {
    let reference = format!("refs/heads/{branch_name}");
    let probe = git_result(repository_root, &["show-ref", "--verify", "--quiet", &reference], &[1]).await?;
    Ok(probe.exit_code == Some(0))
}

async fn require_missing_branch(repository_root: &str, branch_name: &str) -> Result<(), GitWorkspaceError> {
    if branch_exists(repository_root, branch_name).await? {
        return Err(invalid(format!("이미 존재하는 업무 브랜치입니다: {branch_name}")));
    }
    Ok(())
}

/// cleanupFailedProvision. 원래 실패를 유지하며 가능한 정리만 한다.
async fn cleanup_failed_provision(repository_root: &str, worktree_path: &str, branch_name: &str) {
    let _ = git_result(repository_root, &["worktree", "remove", "--force", worktree_path], &[1, 128]).await;
    // worktree가 아직 브랜치를 쓰고 있으면 수동 복구 대상으로 남긴다.
    let _ = git_result(repository_root, &["branch", "-D", branch_name], &[1, 128]).await;
}

async fn require_clean(directory: &str, message: &str, code: GitErrorCode) -> Result<(), GitWorkspaceError> {
    let status = git_buffer(directory, &["status", "--porcelain=v1", "-z", "--untracked-files=all"]).await?;
    if !status.is_empty() {
        return Err(GitWorkspaceError::new(code, message));
    }
    Ok(())
}

/// requireSourceReady의 선택값.
#[derive(Clone, Copy)]
struct SourceReadiness<'a> {
    allow_untracked: bool,
    expected_untracked_paths: Option<&'a [String]>,
    tracked_changes_code: GitErrorCode,
}

impl Default for SourceReadiness<'_> {
    fn default() -> Self {
        Self { allow_untracked: false, expected_untracked_paths: None, tracked_changes_code: GitErrorCode::NotClean }
    }
}

async fn require_source_ready(
    directory: &str,
    message: &str,
    options: SourceReadiness<'_>,
) -> Result<Vec<String>, GitWorkspaceError> {
    let status =
        parse_null_separated(&git_buffer(directory, &["status", "--porcelain=v1", "-z", "--untracked-files=all"]).await?);
    let has_tracked_changes = status.iter().any(|entry| !entry.starts_with("?? "));
    let untracked_paths: Vec<String> =
        status.iter().filter_map(|entry| entry.strip_prefix("?? ")).map(str::to_string).collect();
    if has_tracked_changes || (!options.allow_untracked && !untracked_paths.is_empty()) {
        return Err(GitWorkspaceError::new(options.tracked_changes_code, message));
    }
    if let Some(expected) = options.expected_untracked_paths
        && !same_paths(&untracked_paths, expected)
    {
        return Err(GitWorkspaceError::new(
            GitErrorCode::NotClean,
            "병합 준비 중 원본 Git의 미추적 파일 목록이 달라졌습니다.",
        ));
    }
    Ok(untracked_paths)
}

fn same_paths(left: &[String], right: &[String]) -> bool {
    let right: HashSet<&String> = right.iter().collect();
    left.len() == right.len() && left.iter().all(|path| right.contains(path))
}

async fn tree_paths(directory: &str, tree: &str) -> Result<Vec<String>, GitWorkspaceError> {
    Ok(parse_null_separated(&git_buffer(directory, &["ls-tree", "-r", "--name-only", "-z", tree]).await?))
}

async fn merge_blocking_local_paths(directory: &str) -> Result<Vec<String>, GitWorkspaceError> {
    let untracked = git_buffer(directory, &["ls-files", "--others", "--exclude-standard", "--directory", "-z"]).await?;
    let ignored =
        git_buffer(directory, &["ls-files", "--others", "--ignored", "--exclude-standard", "--directory", "-z"]).await?;
    let mut paths: Vec<String> = Vec::new();
    for path in parse_null_separated(&untracked).into_iter().chain(parse_null_separated(&ignored)) {
        let path = path.strip_suffix('/').map(str::to_string).unwrap_or(path);
        if !paths.contains(&path) {
            paths.push(path);
        }
    }
    Ok(paths)
}

fn normalized_paths_overlap(left: &str, right: &str, ignore_case: bool) -> bool {
    let normalize = |path: &str| {
        let normalized: String = path.nfc().collect();
        if ignore_case { normalized.to_lowercase() } else { normalized }
    };
    let left = normalize(left);
    let right = normalize(right);
    left == right || left.starts_with(&format!("{right}/")) || right.starts_with(&format!("{left}/"))
}

async fn require_merge_tree_paths_available(
    directory: &str,
    source_tree: &str,
    merged_tree: &str,
) -> Result<(), GitWorkspaceError> {
    let source_paths = tree_paths(directory, source_tree).await?;
    let merged_paths = tree_paths(directory, merged_tree).await?;
    let local_paths = merge_blocking_local_paths(directory).await?;
    let ignore_case_result = git_result(directory, &["config", "--bool", "core.ignorecase"], &[1]).await?;
    let source_set: HashSet<&String> = source_paths.iter().collect();
    let added_paths: Vec<&String> = merged_paths.iter().filter(|path| !source_set.contains(path)).collect();
    let ignore_case = js_trim(&ignore_case_result.stdout_text()) == "true";
    let collision = local_paths
        .iter()
        .find(|local| added_paths.iter().any(|added| normalized_paths_overlap(local, added, ignore_case)));
    if let Some(collision) = collision {
        return Err(GitWorkspaceError::new(
            GitErrorCode::NotClean,
            format!("원본 로컬 파일이 승인할 변경 경로와 겹칩니다: {collision}"),
        ));
    }
    Ok(())
}

async fn lstat_if_present(path: &str) -> Result<Option<std::fs::Metadata>, GitWorkspaceError> {
    match tokio::fs::symlink_metadata(path).await {
        Ok(metadata) => Ok(Some(metadata)),
        Err(error) if matches!(error.raw_os_error(), Some(libc::ENOENT) | Some(libc::ENOTDIR)) => Ok(None),
        Err(error) => Err(GitWorkspaceError::filesystem(node_fs_message(&error, "lstat", path))),
    }
}

/// requireNoActiveApprovalHooks. 원본에 미추적 파일이 있으면 실행 가능한 승인 hook이 없어야 한다.
async fn require_no_active_approval_hooks(directory: &str, untracked_paths: &[String]) -> Result<(), GitWorkspaceError> {
    if untracked_paths.is_empty() {
        return Ok(());
    }
    for hook_name in APPROVAL_HOOK_NAMES {
        let git_path = format!("hooks/{hook_name}");
        let hook_path = git_text(directory, &["rev-parse", "--path-format=absolute", "--git-path", &git_path]).await?;
        if let Some(info) = lstat_if_present(&hook_path).await? {
            let file_type = info.file_type();
            if (file_type.is_file() || file_type.is_symlink()) && info.mode() & 0o111 != 0 {
                return Err(GitWorkspaceError::new(
                    GitErrorCode::NotClean,
                    format!("원본에 미추적 파일과 활성 Git hook이 함께 있어 통합할 수 없습니다: {hook_name}"),
                ));
            }
        }
    }
    Ok(())
}

/// localPathFingerprint. 심볼릭 링크는 대상 경로, 일반 파일은 내용의 sha256과 권한으로 지문을 만든다.
async fn local_path_fingerprint(path: &str) -> Result<String, GitWorkspaceError> {
    let info = tokio::fs::symlink_metadata(path)
        .await
        .map_err(|error| GitWorkspaceError::filesystem(node_fs_message(&error, "lstat", path)))?;
    let mode = format!("{:o}", info.mode() & 0o7777);
    let file_type = info.file_type();
    if file_type.is_symlink() {
        let target = tokio::fs::read_link(path)
            .await
            .map_err(|error| GitWorkspaceError::filesystem(node_fs_message(&error, "readlink", path)))?;
        return Ok(format!("symlink:{mode}:{}", sha256_hex(target.to_string_lossy().as_bytes())));
    }
    if !file_type.is_file() {
        return Err(GitWorkspaceError::new(GitErrorCode::NotClean, format!("지원하지 않는 미추적 파일 종류입니다: {path}")));
    }
    let mut file = tokio::fs::File::open(path)
        .await
        .map_err(|error| GitWorkspaceError::filesystem(node_fs_message(&error, "open", path)))?;
    let mut hash = Sha256::new();
    let mut buffer = vec![0_u8; 64 * 1024];
    loop {
        let count = file.read(&mut buffer).await.map_err(|error| {
            let (code, description) = uv_error(&error);
            GitWorkspaceError::filesystem(format!("{code}: {description}, read"))
        })?;
        if count == 0 {
            break;
        }
        hash.update(&buffer[..count]);
    }
    let digest: String = hash.finalize().iter().map(|byte| format!("{byte:02x}")).collect();
    Ok(format!("file:{mode}:{digest}"))
}

async fn fingerprint_untracked_paths(directory: &str, paths: &[String]) -> Result<Vec<(String, String)>, GitWorkspaceError> {
    let mut fingerprints = Vec::with_capacity(paths.len());
    for path in paths {
        fingerprints.push((path.clone(), local_path_fingerprint(&resolve_paths(directory, path)).await?));
    }
    Ok(fingerprints)
}

async fn require_untracked_fingerprints(directory: &str, fingerprints: &[(String, String)]) -> Result<(), GitWorkspaceError> {
    for (path, expected) in fingerprints {
        let changed = || GitWorkspaceError::new(GitErrorCode::NotClean, format!("병합 중 원본 미추적 파일이 달라졌습니다: {path}"));
        let actual = local_path_fingerprint(&resolve_paths(directory, path)).await.map_err(|_| changed())?;
        if &actual != expected {
            return Err(changed());
        }
    }
    Ok(())
}

async fn require_base_ancestor(workspace: &TaskWorkspace, head_commit: &str) -> Result<(), GitWorkspaceError> {
    let result = git_result(
        &workspace.worktree_path,
        &["merge-base", "--is-ancestor", &workspace.base_commit, head_commit],
        &[1],
    )
    .await?;
    if result.exit_code != Some(0) {
        return Err(invalid("업무 브랜치가 생성 기준 커밋에서 이어지지 않습니다."));
    }
    Ok(())
}

/// taskApprovalBase. 업무 변경과 원본 브랜치의 공통 기준이 하나이고 생성 기준에서 이어져야 한다.
async fn task_approval_base(
    workspace: &TaskWorkspace,
    workspace_head: &str,
    source_head: &str,
) -> Result<String, GitWorkspaceError> {
    let merge_base = git_result(&workspace.worktree_path, &["merge-base", "--all", workspace_head, source_head], &[1]).await?;
    let text = merge_base.stdout_text();
    let merge_bases: Vec<&str> = text.split_whitespace().collect();
    if merge_base.exit_code != Some(0) || merge_bases.len() != 1 {
        return Err(invalid("업무 변경과 원본 브랜치의 공통 기준 커밋을 하나로 확인할 수 없습니다."));
    }
    let task_base_commit = merge_bases[0].to_string();
    let original_base = git_result(
        &workspace.worktree_path,
        &["merge-base", "--is-ancestor", &workspace.base_commit, &task_base_commit],
        &[1],
    )
    .await?;
    if original_base.exit_code != Some(0) {
        return Err(invalid("업무 승인 기준이 생성 기준 커밋에서 이어지지 않습니다."));
    }
    Ok(task_base_commit)
}

/// rollbackSourceMerge의 결과.
struct Rollback {
    has_untracked_changes: bool,
}

/// rollbackSourceMerge. 병합 뒤 검증이 실패하면 원본 브랜치를 병합 전 head로 되돌리고 미추적 파일은 보존한다.
async fn rollback_source_merge(
    workspace: &TaskWorkspace,
    source_head: &str,
    baseline_untracked_paths: &[String],
) -> Result<Rollback, GitWorkspaceError> {
    let repository_root = canonical_directory(&workspace.repository_root).await?;
    let root = repository_root.as_str();
    let baseline: HashSet<&String> = baseline_untracked_paths.iter().collect();
    let untracked_before =
        parse_null_separated(&git_buffer(root, &["ls-files", "--others", "--exclude-standard", "-z"]).await?);
    let source_paths: HashSet<String> =
        parse_null_separated(&git_buffer(root, &["ls-tree", "-r", "--name-only", "-z", source_head]).await?)
            .into_iter()
            .collect();
    let has_untracked_collision = untracked_before.iter().any(|path| source_paths.contains(path));

    let base_reference = format!("refs/heads/{}", workspace.base_branch);
    let current_base_commit = git_text(root, &["rev-parse", "--verify", &base_reference]).await?;
    git_text(root, &["update-ref", &base_reference, source_head, &current_base_commit]).await?;
    git_text(root, &["symbolic-ref", "HEAD", &base_reference]).await?;
    if !has_untracked_collision {
        git_text(root, &["restore", "--source", source_head, "--staged", "--worktree", "--", "."]).await?;
    }

    let restored_branch = current_branch(root).await?;
    let restored_head = git_text(root, &["rev-parse", "HEAD"]).await?;
    if restored_branch != workspace.base_branch || restored_head != source_head {
        return Err(GitWorkspaceError::new(GitErrorCode::CommandFailed, "원본 Git 브랜치와 head를 복구하지 못했습니다."));
    }

    let untracked_after: HashSet<String> =
        parse_null_separated(&git_buffer(root, &["ls-files", "--others", "--exclude-standard", "-z"]).await?)
            .into_iter()
            .collect();
    if untracked_before.iter().any(|path| !untracked_after.contains(path)) {
        return Err(GitWorkspaceError::new(GitErrorCode::CommandFailed, "병합 hook의 미추적 산출물을 보존하지 못했습니다."));
    }
    if baseline.iter().any(|path| !untracked_after.contains(*path)) {
        return Err(GitWorkspaceError::new(GitErrorCode::CommandFailed, "병합 전 원본의 미추적 파일을 보존하지 못했습니다."));
    }
    let restored_status =
        parse_null_separated(&git_buffer(root, &["status", "--porcelain=v1", "-z", "--untracked-files=all"]).await?);
    let has_tracked_changes = restored_status.iter().any(|entry| !entry.starts_with("?? "));
    if has_tracked_changes && !has_untracked_collision {
        return Err(GitWorkspaceError::new(GitErrorCode::CommandFailed, "원본 Git tracked 파일과 index를 복구하지 못했습니다."));
    }
    Ok(Rollback {
        has_untracked_changes: has_untracked_collision || untracked_after.iter().any(|path| !baseline.contains(path)),
    })
}

/// merge-tree 결과 첫 줄이 Git 개체 이름(`^[0-9a-f]{40,64}$`)인지 본다.
fn is_object_id(value: &str) -> bool {
    (40..=64).contains(&value.len()) && value.bytes().all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

// ---- 관리자 ----

/// GitWorkspaceManager. 승인은 approval_lock으로 한 번에 하나씩만 처리한다(Node approvalTail).
#[derive(Debug)]
pub struct GitWorkspaceManager {
    source_workdir: String,
    worktree_root: String,
    approval_lock: Mutex<()>,
}

impl GitWorkspaceManager {
    /// worktree_root가 없으면 ~/.officestra/worktrees다. 빈 문자열은 Node처럼 현재 폴더로 풀린다.
    pub fn new(source_workdir: &str, worktree_root: Option<&str>) -> Result<Self, GitWorkspaceError> {
        if source_workdir.is_empty() {
            return Err(invalid("원본 작업 폴더가 필요합니다."));
        }
        let worktree_root = match worktree_root {
            Some(root) => resolve_path(root),
            None => resolve_path(&join_path(&[&home_directory(), ".officestra", "worktrees"])),
        };
        Ok(Self { source_workdir: resolve_path(source_workdir), worktree_root, approval_lock: Mutex::new(()) })
    }

    pub fn source_workdir(&self) -> &str {
        &self.source_workdir
    }

    pub fn worktree_root(&self) -> &str {
        &self.worktree_root
    }

    pub async fn is_repository(&self) -> Result<bool, GitWorkspaceError> {
        let source_workdir = canonical_directory(&self.source_workdir).await?;
        Ok(repository_root_for(&source_workdir).await?.is_some())
    }

    /// planProvision. 원본이 Git 저장소가 아니면 None이다.
    pub async fn plan_provision(
        &self,
        workspace_id: &str,
        character_id: &str,
    ) -> Result<Option<TaskWorkspace>, GitWorkspaceError> {
        let source_workdir = canonical_directory(&self.source_workdir).await?;
        let Some(repository_root) = repository_root_for(&source_workdir).await? else {
            return Ok(None);
        };
        if !is_outside_directory(&relative_path(&repository_root, &self.worktree_root)) {
            return Err(invalid("worktree 보관 폴더는 원본 Git 저장소 밖에 있어야 합니다."));
        }
        let source_relative_path = relative_path(&repository_root, &source_workdir);
        if is_outside_directory(&source_relative_path) {
            return Err(invalid("원본 작업 폴더가 Git 저장소 밖에 있습니다."));
        }
        let base_branch = current_branch(&repository_root).await?;
        let base_commit = git_text(&repository_root, &["rev-parse", "HEAD"]).await?;
        let branch_name = workspace_branch_name(character_id, workspace_id);
        require_missing_branch(&repository_root, &branch_name).await?;

        let leaf_name = format!("{}-{}", safe_segment(character_id), safe_segment(workspace_id));
        let worktree_path = join_path(&[&self.worktree_root, &repository_key(&repository_root), &leaf_name]);
        if path_exists(&worktree_path).await {
            return Err(invalid(format!("이미 존재하는 업무 worktree 경로입니다: {worktree_path}")));
        }
        let execution_workdir = if source_relative_path.is_empty() {
            worktree_path.clone()
        } else {
            join_path(&[&worktree_path, &source_relative_path])
        };
        Ok(Some(TaskWorkspace {
            repository_root,
            source_workdir,
            worktree_path,
            execution_workdir,
            branch_name,
            base_branch,
            base_commit,
            ..TaskWorkspace::default()
        }))
    }

    /// provisionPlanned. worktree를 만든 뒤 경로가 계획과 다르면 만든 것을 지우고 실패한다.
    pub async fn provision_planned(&self, plan: &TaskWorkspace) -> Result<TaskWorkspace, GitWorkspaceError> {
        validate_workspace_shape(plan)?;
        let repository_root = canonical_directory(&plan.repository_root).await?;
        let source_workdir = canonical_directory(&plan.source_workdir).await?;
        if repository_root != plan.repository_root || source_workdir != plan.source_workdir {
            return Err(invalid("계획한 Git 작업 경로가 현재 경로와 다릅니다."));
        }
        if current_branch(&repository_root).await? != plan.base_branch {
            return Err(invalid("작업 공간 준비 중 원본 브랜치가 변경됐습니다."));
        }
        if git_text(&repository_root, &["rev-parse", "HEAD"]).await? != plan.base_commit {
            return Err(invalid("작업 공간 준비 중 원본 커밋이 변경됐습니다."));
        }
        require_missing_branch(&repository_root, &plan.branch_name).await?;
        if path_exists(&plan.worktree_path).await {
            return Err(invalid(format!("이미 존재하는 업무 worktree 경로입니다: {}", plan.worktree_path)));
        }

        let parent = resolve_paths(&plan.worktree_path, "..");
        tokio::fs::create_dir_all(&parent)
            .await
            .map_err(|error| GitWorkspaceError::filesystem(node_fs_message(&error, "mkdir", &parent)))?;
        let added = async {
            git_text(
                &repository_root,
                &["worktree", "add", "-b", &plan.branch_name, &plan.worktree_path, &plan.base_commit],
            )
            .await?;
            let worktree_path = canonical_directory(&plan.worktree_path).await?;
            let execution_workdir = canonical_directory(&plan.execution_workdir).await?;
            if worktree_path != plan.worktree_path || execution_workdir != plan.execution_workdir {
                return Err(invalid("생성한 Git 작업 경로가 계획한 경로와 다릅니다."));
            }
            Ok(())
        }
        .await;
        if let Err(error) = added {
            cleanup_failed_provision(&repository_root, &plan.worktree_path, &plan.branch_name).await;
            return Err(error);
        }
        Ok(plan.clone())
    }

    pub async fn provision(&self, workspace_id: &str, character_id: &str) -> Result<Option<TaskWorkspace>, GitWorkspaceError> {
        match self.plan_provision(workspace_id, character_id).await? {
            Some(plan) => Ok(Some(self.provision_planned(&plan).await?)),
            None => Ok(None),
        }
    }

    /// cleanupProvisioning. 이 manager가 계획했을 경로·브랜치일 때만 강제로 지운다.
    pub async fn cleanup_provisioning(&self, plan: &TaskWorkspace) -> Result<(), GitWorkspaceError> {
        validate_workspace_shape(plan)?;
        let source_workdir = canonical_directory(&self.source_workdir).await?;
        let repository_root = canonical_directory(&plan.repository_root).await?;
        let manager_repository_root = repository_root_for(&source_workdir).await?;
        let relative_worktree_path = relative_path(&self.worktree_root, &plan.worktree_path);
        let worktree_segments: Vec<&str> =
            relative_worktree_path.split(['\\', '/']).filter(|segment| !segment.is_empty()).collect();
        if repository_root != plan.repository_root
            || source_workdir != plan.source_workdir
            || manager_repository_root.as_deref() != Some(repository_root.as_str())
            || !plan.branch_name.starts_with("officestra/")
            || plan.branch_name == plan.base_branch
            || is_outside_directory(&relative_worktree_path)
            || worktree_segments.len() != 2
            || worktree_segments[0] != repository_key(&repository_root)
        {
            return Err(invalid("정리할 provisioning 작업 공간이 현재 manager 계획과 다릅니다."));
        }
        cleanup_failed_provision(&repository_root, &plan.worktree_path, &plan.branch_name).await;
        let branch_remains = branch_exists(&repository_root, &plan.branch_name).await?;
        if path_exists(&plan.worktree_path).await || branch_remains {
            return Err(GitWorkspaceError::new(
                GitErrorCode::CommandFailed,
                "중단된 Git 작업 공간을 자동으로 정리하지 못했습니다.",
            ));
        }
        Ok(())
    }

    /// prepareReview. worktree 변경을 모두 index에 올려 검토할 tree를 만든다.
    pub async fn prepare_review(&self, workspace: &TaskWorkspace) -> Result<WorkspaceReview, GitWorkspaceError> {
        self.validate_source(workspace, None, None).await?;
        self.validate_workspace(workspace).await?;
        let worktree = workspace.worktree_path.as_str();
        git_text(worktree, &["add", "-A", "--", "."]).await?;
        let review_tree = git_text(worktree, &["write-tree"]).await?;
        let head_commit = git_text(worktree, &["rev-parse", "HEAD"]).await?;
        require_base_ancestor(workspace, &head_commit).await?;
        let base_tree =
            git_text(&workspace.repository_root, &["rev-parse", &format!("{}^{{tree}}", workspace.base_commit)]).await?;
        let changed_files = if review_tree == base_tree {
            Vec::new()
        } else {
            parse_changed_files(
                &git_buffer(
                    worktree,
                    &["diff", "--cached", "--name-status", "-z", "--find-renames", &workspace.base_commit, "--"],
                )
                .await?,
            )
        };
        require_unchanged_frozen_work_records(&changed_files)?;
        Ok(WorkspaceReview { has_changes: review_tree != base_tree, review_tree, head_commit, changed_files })
    }

    /// diff. 검토 tree가 있으면 기준 커밋과 그 tree를, 없으면 worktree index를 비교한다.
    pub async fn diff(&self, workspace: &TaskWorkspace, max_bytes: usize) -> Result<DiffOutput, GitWorkspaceError> {
        validate_workspace_shape(workspace)?;
        if max_bytes < 1 {
            return Err(invalid("diff 최대 크기는 1 이상의 정수여야 합니다."));
        }
        let review_tree = js_trim(workspace.review_tree.as_deref().unwrap_or(""));
        if !review_tree.is_empty() {
            let repository_root = canonical_directory(&workspace.repository_root).await?;
            git_text(&repository_root, &["cat-file", "-e", &format!("{review_tree}^{{tree}}")]).await?;
            return limited_git_output(
                &repository_root,
                &[
                    "diff",
                    "--no-color",
                    "--no-ext-diff",
                    "--find-renames",
                    "--src-prefix=a/",
                    "--dst-prefix=b/",
                    &workspace.base_commit,
                    review_tree,
                    "--",
                ],
                max_bytes,
            )
            .await;
        }
        self.validate_workspace(workspace).await?;
        limited_git_output(
            &workspace.worktree_path,
            &[
                "diff",
                "--cached",
                "--no-color",
                "--no-ext-diff",
                "--find-renames",
                "--src-prefix=a/",
                "--dst-prefix=b/",
                &workspace.base_commit,
                "--",
            ],
            max_bytes,
        )
        .await
    }

    /// approve. 검토한 tree만 한 커밋으로 묶어 원본 브랜치에 병합한다.
    pub async fn approve(
        &self,
        workspace: &TaskWorkspace,
        expected_review_tree: &str,
        commit_message: Option<&str>,
    ) -> Result<ApprovedMerge, GitWorkspaceError> {
        let _guard = self.approval_lock.lock().await;
        self.approve_locked(workspace, expected_review_tree, commit_message).await
    }

    async fn approve_locked(
        &self,
        workspace: &TaskWorkspace,
        expected_review_tree: &str,
        commit_message: Option<&str>,
    ) -> Result<ApprovedMerge, GitWorkspaceError> {
        if js_trim(expected_review_tree).is_empty() {
            return Err(invalid("검토한 Git tree 값이 필요합니다."));
        }
        validate_workspace_shape(workspace)?;
        if let Some(recovered) = self.already_merged(workspace, expected_review_tree).await? {
            return Ok(recovered);
        }
        self.validate_workspace(workspace).await?;
        let root = workspace.repository_root.as_str();
        let worktree = workspace.worktree_path.as_str();
        let untracked_readiness = SourceReadiness { allow_untracked: true, ..SourceReadiness::default() };
        let source_untracked_paths = self.validate_source(workspace, None, Some(untracked_readiness)).await?;
        require_no_active_approval_hooks(root, &source_untracked_paths).await?;
        let source_untracked_fingerprints = fingerprint_untracked_paths(root, &source_untracked_paths).await?;
        git_text(worktree, &["add", "-A", "--", "."]).await?;
        let current_tree = git_text(worktree, &["write-tree"]).await?;
        if current_tree != expected_review_tree {
            return Err(GitWorkspaceError::new(
                GitErrorCode::ChangedAfterReview,
                "검토 후 업무 변경사항이 달라졌습니다. 변경사항을 다시 확인하세요.",
            ));
        }

        let workspace_head = git_text(worktree, &["rev-parse", "HEAD"]).await?;
        require_base_ancestor(workspace, &workspace_head).await?;
        let source_head = git_text(root, &["rev-parse", "HEAD"]).await?;
        let task_base_commit = task_approval_base(workspace, &workspace_head, &source_head).await?;
        require_unchanged_frozen_work_records(&parse_changed_files(
            &git_buffer(worktree, &["diff", "--cached", "--name-status", "-z", "--find-renames", &task_base_commit, "--"])
                .await?,
        ))?;
        let base_tree = git_text(root, &["rev-parse", &format!("{task_base_commit}^{{tree}}")]).await?;
        if current_tree == base_tree {
            return Err(invalid("병합할 변경사항이 없습니다."));
        }

        git_text(worktree, &["reset", "--soft", &task_base_commit]).await?;
        let message = commit_message.map(js_trim).filter(|message| !message.is_empty());
        let message = message.unwrap_or("OFFICESTRA: approve workspace changes");
        git_text(
            worktree,
            &["-c", "user.name=OFFICESTRA", "-c", "user.email=officestra@local", "commit", "--no-gpg-sign", "-m", message],
        )
        .await?;
        let task_commit = git_text(worktree, &["rev-parse", "HEAD"]).await?;
        let task_tree = git_text(worktree, &["rev-parse", &format!("{task_commit}^{{tree}}")]).await?;
        if task_tree != expected_review_tree {
            return Err(GitWorkspaceError::new(
                GitErrorCode::ChangedAfterReview,
                "커밋 hook이 검토한 변경사항을 바꿨습니다. 변경사항을 다시 확인하세요.",
            ));
        }
        let task_lineage = git_text(worktree, &["rev-list", "--parents", "-n", "1", &task_commit]).await?;
        let task_lineage: Vec<&str> = task_lineage.split_whitespace().collect();
        if task_lineage.len() != 2 || task_lineage[1] != task_base_commit {
            return Err(invalid("승인 커밋이 기록한 기준 커밋에서 직접 이어지지 않습니다."));
        }
        require_clean(worktree, "업무 worktree에 검토되지 않은 변경사항이 있습니다.", GitErrorCode::ChangedAfterReview)
            .await?;

        let merge_check = git_result(root, &["merge-tree", "--write-tree", &source_head, &task_commit], &[1]).await?;
        if merge_check.exit_code == Some(1) {
            return Err(GitWorkspaceError::new(GitErrorCode::Conflict, "원본 브랜치의 최신 변경과 업무 변경이 충돌합니다."));
        }
        let merge_output = merge_check.stdout_text();
        let expected_merge_tree = js_trim(merge_output.split('\n').next().unwrap_or("")).to_string();
        if !is_object_id(&expected_merge_tree) {
            return Err(GitWorkspaceError::new(GitErrorCode::CommandFailed, "Git 병합 결과 tree를 확인할 수 없습니다."));
        }
        require_merge_tree_paths_available(root, &source_head, &expected_merge_tree).await?;
        let expected_readiness = SourceReadiness {
            allow_untracked: true,
            expected_untracked_paths: Some(&source_untracked_paths),
            ..SourceReadiness::default()
        };
        self.validate_source(workspace, Some(&source_head), Some(expected_readiness)).await?;
        require_no_active_approval_hooks(root, &source_untracked_paths).await?;
        require_untracked_fingerprints(root, &source_untracked_fingerprints).await?;
        let merge_result = git_result(
            root,
            &[
                "-c",
                "user.name=OFFICESTRA",
                "-c",
                "user.email=officestra@local",
                "merge",
                "--no-ff",
                "--no-edit",
                "--no-gpg-sign",
                &task_commit,
            ],
            &[1, 2, 128],
        )
        .await?;
        if merge_result.exit_code != Some(0) {
            let merge_head = git_result(root, &["rev-parse", "--quiet", "--verify", "MERGE_HEAD"], &[1, 128]).await?;
            if merge_head.exit_code == Some(0) {
                git_result(root, &["merge", "--abort"], &[1, 128]).await?;
            }
            return Err(command_error("Git 병합에 실패했습니다.", &merge_result));
        }

        let verified = async {
            let merged_commit = git_text(root, &["rev-parse", "HEAD"]).await?;
            let merged_tree = git_text(root, &["rev-parse", &format!("{merged_commit}^{{tree}}")]).await?;
            let merged_lineage = git_text(root, &["rev-list", "--parents", "-n", "1", &merged_commit]).await?;
            let merged_lineage: Vec<&str> = merged_lineage.split_whitespace().collect();
            require_source_ready(
                root,
                "병합 중 원본 Git 작업 트리의 변경사항이 달라졌습니다.",
                SourceReadiness {
                    allow_untracked: true,
                    expected_untracked_paths: Some(&source_untracked_paths),
                    tracked_changes_code: GitErrorCode::CommandFailed,
                },
            )
            .await?;
            require_untracked_fingerprints(root, &source_untracked_fingerprints).await?;
            if merged_tree != expected_merge_tree
                || merged_lineage.len() != 3
                || merged_lineage[1] != source_head
                || merged_lineage[2] != task_commit
            {
                return Err(GitWorkspaceError::new(
                    GitErrorCode::CommandFailed,
                    "병합 hook이 검토한 tree, Git 이력 또는 원본 작업 파일을 바꿨습니다.",
                ));
            }
            Ok(merged_commit)
        }
        .await;
        let error = match verified {
            Ok(merged_commit) => return Ok(ApprovedMerge { task_commit, merged_commit }),
            Err(error) => error,
        };
        let rollback = rollback_source_merge(workspace, &source_head, &source_untracked_paths).await.map_err(|rollback| {
            GitWorkspaceError::new(
                GitErrorCode::CommandFailed,
                format!("통합 검증과 원본 브랜치 복구에 실패했습니다. {}", rollback.message),
            )
        })?;
        if rollback.has_untracked_changes {
            return Err(GitWorkspaceError::new(
                GitErrorCode::NotClean,
                "병합 hook의 미추적 산출물은 보존했습니다. 원본 브랜치는 되돌렸으니 파일을 확인한 뒤 다시 승인하세요.",
            ));
        }
        if error.code == Some(GitErrorCode::NotClean) {
            return Err(GitWorkspaceError::new(GitErrorCode::NotClean, error.message));
        }
        Err(GitWorkspaceError::new(
            GitErrorCode::CommandFailed,
            "병합 hook이 검토 결과를 바꿔 원본 브랜치와 tracked 파일을 되돌렸습니다.",
        ))
    }

    /// alreadyMerged. 승인 커밋을 만든 뒤 DB 기록 전에 멈췄다면 그 커밋을 다시 찾아 돌려준다.
    async fn already_merged(
        &self,
        workspace: &TaskWorkspace,
        expected_review_tree: &str,
    ) -> Result<Option<ApprovedMerge>, GitWorkspaceError> {
        let root = workspace.repository_root.as_str();
        let base_reference = format!("refs/heads/{}", workspace.base_branch);
        let target = git_result(root, &["rev-parse", "--verify", &base_reference], &[128]).await?;
        if target.exit_code != Some(0) {
            return Ok(None);
        }
        let merged_commit = js_trim(&target.stdout_text()).to_string();
        let mut candidates: Vec<String> = Vec::new();
        if let Some(task_commit) = workspace.task_commit.as_deref().filter(|commit| !commit.is_empty()) {
            candidates.push(task_commit.to_string());
        }
        let task_reference = format!("refs/heads/{}", workspace.branch_name);
        let task_branch = git_result(root, &["rev-parse", "--verify", &task_reference], &[128]).await?;
        if task_branch.exit_code == Some(0) {
            let head = js_trim(&task_branch.stdout_text()).to_string();
            if !head.is_empty() && !candidates.contains(&head) {
                candidates.push(head);
            }
        }
        for task_commit in candidates {
            let tree = git_result(root, &["rev-parse", "--verify", &format!("{task_commit}^{{tree}}")], &[128]).await?;
            if tree.exit_code != Some(0) || js_trim(&tree.stdout_text()) != expected_review_tree {
                continue;
            }
            let ancestor = git_result(root, &["merge-base", "--is-ancestor", &task_commit, &merged_commit], &[1]).await?;
            if ancestor.exit_code == Some(0) {
                return Ok(Some(ApprovedMerge { task_commit, merged_commit }));
            }
        }
        Ok(None)
    }

    /// cleanup. worktree와 업무 브랜치를 지운다. 브랜치는 병합된 경우에만 지워진다(`branch -d`).
    pub async fn cleanup(&self, workspace: &TaskWorkspace) -> Result<(), GitWorkspaceError> {
        validate_workspace_shape(workspace)?;
        let repository_root = canonical_directory(&workspace.repository_root).await?;
        if path_exists(&workspace.worktree_path).await {
            self.validate_workspace(workspace).await?;
            git_text(&repository_root, &["worktree", "remove", &workspace.worktree_path]).await?;
        }
        if branch_exists(&repository_root, &workspace.branch_name).await? {
            git_text(&repository_root, &["branch", "-d", &workspace.branch_name]).await?;
        }
        Ok(())
    }

    /// validateWorkspace. 저장된 경로·저장소 연결·브랜치가 지금도 같은지 확인한다.
    pub async fn validate_workspace(&self, workspace: &TaskWorkspace) -> Result<(), GitWorkspaceError> {
        validate_workspace_shape(workspace)?;
        let repository_root = canonical_directory(&workspace.repository_root).await?;
        let worktree_path = canonical_directory(&workspace.worktree_path).await?;
        if repository_root != workspace.repository_root || worktree_path != workspace.worktree_path {
            return Err(invalid("저장된 Git workspace 경로가 현재 경로와 다릅니다."));
        }
        let actual_root = canonical_directory(&git_text(&worktree_path, &["rev-parse", "--show-toplevel"]).await?).await?;
        if actual_root != worktree_path {
            return Err(invalid("업무 worktree 루트가 예상 경로와 다릅니다."));
        }
        let common_directory = ["rev-parse", "--path-format=absolute", "--git-common-dir"];
        let expected_common_directory = git_text(&repository_root, &common_directory).await?;
        let actual_common_directory = git_text(&worktree_path, &common_directory).await?;
        if resolve_path(&expected_common_directory) != resolve_path(&actual_common_directory) {
            return Err(invalid("업무 worktree가 예상 Git 저장소에 연결되어 있지 않습니다."));
        }
        if current_branch(&worktree_path).await? != workspace.branch_name {
            return Err(invalid("업무 worktree 브랜치가 변경됐습니다."));
        }
        Ok(())
    }

    /// validateSource. readiness가 None이면 Node allowDirty처럼 브랜치만 본다.
    async fn validate_source(
        &self,
        workspace: &TaskWorkspace,
        expected_head: Option<&str>,
        readiness: Option<SourceReadiness<'_>>,
    ) -> Result<Vec<String>, GitWorkspaceError> {
        if current_branch(&workspace.repository_root).await? != workspace.base_branch {
            return Err(invalid("원본 작업 트리의 브랜치가 변경됐습니다."));
        }
        let mut untracked_paths = Vec::new();
        if let Some(readiness) = readiness {
            untracked_paths = require_source_ready(
                &workspace.repository_root,
                "원본 Git 작업 트리에 변경사항이 있어 병합할 수 없습니다.",
                readiness,
            )
            .await?;
        }
        if let Some(expected_head) = expected_head.filter(|head| !head.is_empty())
            && git_text(&workspace.repository_root, &["rev-parse", "HEAD"]).await? != expected_head
        {
            return Err(invalid("병합 준비 중 원본 브랜치가 변경됐습니다."));
        }
        Ok(untracked_paths)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn safe_segment_matches_node_rules() {
        assert_eq!(safe_segment("  Left Woman!! "), "left-woman");
        assert_eq!(safe_segment("..--abc--.."), "abc");
        assert_eq!(safe_segment("한글"), "workspace");
        assert_eq!(safe_segment("a/b\\c"), "a-b-c");
        assert_eq!(safe_segment(&"x".repeat(80)).len(), 64);
    }

    #[test]
    fn branch_name_uses_sha_suffix_and_48_character_workspace() {
        let workspace_id = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789";
        let name = workspace_branch_name("Left-Woman", workspace_id);
        let suffix = &sha256_hex(format!("Left-Woman\0{workspace_id}").as_bytes())[..8];
        assert_eq!(name, format!("officestra/left-woman/{}-{suffix}", &workspace_id[..48]));
    }

    #[test]
    fn changed_files_keep_rename_order_and_frozen_paths_are_rejected() {
        let files = parse_changed_files(b"M\0a.txt\0R100\0old.md\0checklist.md\0");
        assert_eq!(
            files,
            vec![
                json!({"status": "M", "path": "a.txt"}),
                json!({"status": "R100", "path": "checklist.md", "previousPath": "old.md"}),
            ]
        );
        let error = require_unchanged_frozen_work_records(&files).unwrap_err();
        assert_eq!(error.message, "v1.0 동결 작업 기록은 변경할 수 없습니다: checklist.md");
        assert_eq!(error.code, Some(GitErrorCode::InvalidState));
    }

    #[test]
    fn path_overlap_uses_nfc_and_ignore_case() {
        assert!(normalized_paths_overlap("docs", "docs/a.md", false));
        assert!(normalized_paths_overlap("Docs/A.md", "docs", true));
        assert!(!normalized_paths_overlap("Docs/A.md", "docs", false));
        assert!(normalized_paths_overlap("cafe\u{301}", "caf\u{e9}", false));
        assert!(!normalized_paths_overlap("doc", "docs", false));
        assert_eq!(dirname("/a/b"), "/a");
        assert_eq!(dirname("/a"), "/");
        assert_eq!(dirname("/"), "/");
    }
}
