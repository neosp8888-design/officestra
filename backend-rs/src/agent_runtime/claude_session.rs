// Claude Code 세션 기록 찾기와 작업 공간이 바뀐 재개 준비. Node agent-runtime.mjs의 claudeSessionPath~sameFileIdentity와 같다.

use std::cmp::Ordering;
use std::io::{Read, Seek, SeekFrom};
use std::os::unix::fs::MetadataExt;
use std::path::Path;

use serde_json::Value;

use crate::js::{date_parse, join_path, js_trim, node_fs_message, parse_js_json, realpath_sync, uv_error};

const MISSING_SESSION: &str = "저장된 Claude Code 세션 기록을 찾을 수 없습니다. 대화 이력을 버리고 새 세션으로 바꾸지 않았습니다.";
const DIFFERENT_SESSION: &str = "현재 작업 공간에 서로 다른 Claude Code 세션 기록이 이미 있습니다. 기존 기록을 덮어쓰거나 숨기지 않았습니다.";
const SIDECAR_NOT_DIRECTORY: &str = "Claude Code 세션 부속 기록 경로가 디렉토리가 아닙니다.";
const DIFFERENT_SIDECAR: &str = "현재 작업 공간에 서로 다른 Claude Code 세션 부속 기록이 있습니다. 기존 기록을 덮어쓰거나 숨기지 않았습니다.";
const MESSAGE_SCAN_BYTES: u64 = 4 * 1024 * 1024;
const SESSION_PREFIX_BYTES: u64 = 64 * 1024;

/// claudeSessionPath. `projects_root`는 Node의 `~/.claude/projects`이고, 실제 경로의 `/`·`.`을 `-`로 바꾼 폴더 아래다.
pub fn claude_session_path(projects_root: &str, workdir: Option<&str>, session_id: &str) -> Option<String> {
    let directory = js_trim(workdir.unwrap_or(""));
    let id = js_trim(session_id);
    if directory.is_empty() || id.is_empty() {
        return None;
    }
    let resolved = realpath_sync(directory).unwrap_or_else(|_| directory.to_string());
    Some(join_path(&[projects_root, &resolved.replace(['/', '.'], "-"), &format!("{id}.jsonl")]))
}

/// claudeSessionResumable.
pub fn claude_session_resumable(projects_root: &str, workdir: Option<&str>, session_id: &str) -> bool {
    claude_session_path(projects_root, workdir, session_id).is_some_and(|path| Path::new(&path).exists())
}

/// findClaudeSessionPath. 같은 ID의 기록이 여러 작업 공간에 있으면 마지막 실제 메시지가 최신인 것을 고른다.
pub fn find_claude_session_path(projects_root: &str, session_id: &str) -> Option<String> {
    claude_session_candidates(projects_root, session_id).into_iter().next().map(|candidate| candidate.path)
}

/// 중단된 Claude 턴의 사용량 복구에 쓸 기록. 현재 작업 공간 기록이 이 세션이면 그것을, 아니면 가장 최신 기록이다.
pub fn claude_usage_session_path(projects_root: &str, workdir: Option<&str>, session_id: &str) -> Option<String> {
    let current = claude_session_path(projects_root, workdir, session_id);
    if claude_session_file_matches(current.as_deref(), session_id) {
        return current;
    }
    find_claude_session_path(projects_root, session_id)
}

struct Candidate {
    path: String,
    size: u64,
    modified_at: f64,
    message_at: Option<f64>,
}

fn claude_session_candidates(projects_root: &str, session_id: &str) -> Vec<Candidate> {
    let id = js_trim(session_id);
    if id.is_empty() || !Path::new(projects_root).exists() {
        return Vec::new();
    }
    let Ok(Ok(directories)) = std::fs::read_dir(projects_root).map(|entries| entries.collect::<Result<Vec<_>, _>>()) else {
        return Vec::new();
    };
    let mut candidates: Vec<Candidate> = directories
        .into_iter()
        .filter_map(|directory| {
            let path = join_path(&[projects_root, &directory.file_name().to_string_lossy(), &format!("{id}.jsonl")]);
            // 다른 작업 공간이 동시에 정리한 후보는 건너뛴다.
            let metadata = std::fs::metadata(&path).ok().filter(std::fs::Metadata::is_file)?;
            Some(Candidate {
                size: metadata.len(),
                modified_at: metadata.mtime() as f64 * 1000.0 + metadata.mtime_nsec() as f64 / 1_000_000.0,
                message_at: latest_claude_message_timestamp(&path),
                path,
            })
        })
        .collect();
    let descending = |left: f64, right: f64| {
        let difference = right - left;
        (difference != 0.0 && !difference.is_nan()).then(|| if difference < 0.0 { Ordering::Less } else { Ordering::Greater })
    };
    candidates.sort_by(|left, right| {
        let message = |candidate: &Candidate| candidate.message_at.unwrap_or(f64::NEG_INFINITY);
        descending(message(left), message(right))
            .or_else(|| descending(left.modified_at, right.modified_at))
            .or_else(|| descending(left.size as f64, right.size as f64))
            // Node는 localeCompare로 고른다. 여기서는 session_context_usage와 같이 바이트 순서로 둔다.
            .unwrap_or_else(|| left.path.cmp(&right.path))
    });
    candidates
}

/// prepareClaudeSessionResume. 작업 공간이 바뀌었으면 이전 기록을 새 위치에 하드 링크해 같은 기록으로 이어 쓴다.
/// 기록이 없거나 다른 분기가 이미 있으면 새 세션으로 바꾸지 않고 실패한다. 업무 폴더가 없으면 None이다.
pub fn prepare_claude_session_resume(
    projects_root: &str,
    session_id: &str,
    workdir: Option<&str>,
    previous_workdir: Option<&str>,
) -> Result<Option<String>, String> {
    let Some(target) = claude_session_path(projects_root, workdir, session_id) else {
        return Ok(None);
    };
    let preferred = claude_session_path(projects_root, previous_workdir, session_id);
    let source = if claude_session_file_matches(preferred.as_deref(), session_id) {
        preferred
    } else {
        find_claude_session_path(projects_root, session_id)
    };
    let Some(source) = source else {
        return Err(MISSING_SESSION.into());
    };
    if source == target {
        return Ok(Some(source));
    }
    if Path::new(&target).exists() && !same_file_identity(&source, &target) {
        return Err(DIFFERENT_SESSION.into());
    }
    let sidecar = prepare_claude_session_sidecar(&source, &target, session_id)?;
    if Path::new(&target).exists() {
        return Ok(Some(target));
    }
    let directory = dirname(&target);
    std::fs::create_dir_all(directory).map_err(|error| node_fs_message(&error, "mkdir", directory))?;
    match std::fs::hard_link(&source, &target) {
        Ok(()) => Ok(Some(target)),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists && same_file_identity(&source, &target) => Ok(Some(target)),
        Err(error) => {
            // 만든 부속 기록 링크만 최선으로 되돌리고 원래 파일은 건드리지 않는다.
            if let Some(path) = sidecar {
                let _ = std::fs::remove_file(path);
            }
            Err(node_link_message(&error, "link", &source, &target))
        }
    }
}

/// prepareClaudeSessionSidecar. 하위 에이전트·도구 결과 폴더를 새 위치에 디렉토리 링크로 잇는다. 새로 만든 링크 경로를 돌려준다.
fn prepare_claude_session_sidecar(source: &str, target: &str, session_id: &str) -> Result<Option<String>, String> {
    let source_path = join_path(&[dirname(source), session_id]);
    if !path_entry_exists(&source_path) {
        return Ok(None);
    }
    let metadata = std::fs::metadata(&source_path).map_err(|error| node_fs_message(&error, "stat", &source_path))?;
    if !metadata.is_dir() {
        return Err(SIDECAR_NOT_DIRECTORY.into());
    }
    let target_path = join_path(&[dirname(target), session_id]);
    if path_entry_exists(&target_path) {
        return if same_real_path(&target_path, &source_path) { Ok(None) } else { Err(DIFFERENT_SIDECAR.into()) };
    }
    let directory = dirname(&target_path);
    std::fs::create_dir_all(directory).map_err(|error| node_fs_message(&error, "mkdir", directory))?;
    let resolved_source = realpath_sync(&source_path)?;
    match std::os::unix::fs::symlink(&resolved_source, &target_path) {
        Ok(()) => Ok(Some(target_path)),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists && same_real_path(&target_path, &resolved_source) => Ok(None),
        Err(error) => Err(node_link_message(&error, "symlink", &resolved_source, &target_path)),
    }
}

/// Node 두 경로 fs 오류 문구 `EXDEV: cross-device link not permitted, link '<원본>' -> '<대상>'`.
fn node_link_message(error: &std::io::Error, syscall: &str, source: &str, target: &str) -> String {
    let (code, description) = uv_error(error);
    format!("{code}: {description}, {syscall} '{source}' -> '{target}'")
}

fn dirname(path: &str) -> &str {
    match path.rfind('/') {
        Some(0) => "/",
        Some(index) => &path[..index],
        None => ".",
    }
}

fn path_entry_exists(path: &str) -> bool {
    std::fs::symlink_metadata(path).is_ok()
}

fn same_real_path(left: &str, right: &str) -> bool {
    matches!((realpath_sync(left), realpath_sync(right)), (Ok(left), Ok(right)) if left == right)
}

/// sameFileIdentity. 하드 링크로 이은 같은 기록인지 장치·inode로 본다.
fn same_file_identity(left: &str, right: &str) -> bool {
    match (std::fs::metadata(left), std::fs::metadata(right)) {
        (Ok(left), Ok(right)) => left.dev() == right.dev() && left.ino() == right.ino(),
        _ => false,
    }
}

fn read_range(path: &str, offset: u64, length: u64) -> std::io::Result<Vec<u8>> {
    let mut file = std::fs::File::open(path)?;
    file.seek(SeekFrom::Start(offset))?;
    let mut buffer = Vec::new();
    file.take(length).read_to_end(&mut buffer)?;
    Ok(buffer)
}

/// latestClaudeMessageTimestamp. 끝 4MiB에서 곁가지가 아닌 마지막 user·assistant 메시지 시각(밀리초)이다.
fn latest_claude_message_timestamp(path: &str) -> Option<f64> {
    let size = std::fs::metadata(path).ok()?.len();
    let offset = size.saturating_sub(MESSAGE_SCAN_BYTES);
    let buffer = read_range(path, offset, size - offset).ok()?;
    let text = String::from_utf8_lossy(&buffer);
    let lines = text.split('\n').skip(usize::from(offset > 0)).collect::<Vec<_>>();
    lines.into_iter().rev().find_map(|line| {
        if !line.contains("\"timestamp\"") || (!line.contains("\"type\":\"assistant\"") && !line.contains("\"type\":\"user\"")) {
            return None;
        }
        let event = parse_js_json(line).ok()?;
        if event.get("isSidechain") == Some(&Value::Bool(true)) {
            return None;
        }
        event.get("timestamp").and_then(Value::as_str).and_then(date_parse).filter(|at| at.is_finite())
    })
}

/// claudeSessionFileMatches. 앞 64KiB에 이 세션 ID가 적힌 일반 파일인지 본다.
pub(crate) fn claude_session_file_matches(path: Option<&str>, session_id: &str) -> bool {
    let Some(path) = path else {
        return false;
    };
    let Ok(metadata) = std::fs::metadata(path) else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    let Ok(buffer) = read_range(path, 0, metadata.len().min(SESSION_PREFIX_BYTES)) else {
        return false;
    };
    let prefix = String::from_utf8_lossy(&buffer);
    prefix.contains(&format!("\"sessionId\":\"{session_id}\"")) || prefix.contains(&format!("\"session_id\":\"{session_id}\""))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::js::{node_tmpdir, random_uuid};

    struct Home {
        projects: String,
        directories: Vec<String>,
    }

    impl Home {
        fn new() -> Self {
            let projects = format!("{}/office-claude-home-{}/.claude/projects", node_tmpdir(), random_uuid());
            Self { projects, directories: Vec::new() }
        }

        fn workdir(&mut self, prefix: &str) -> String {
            let path = format!("{}/{prefix}{}", node_tmpdir(), random_uuid());
            std::fs::create_dir_all(&path).unwrap();
            self.directories.push(path.clone());
            path
        }

        fn path(&self, workdir: &str) -> String {
            claude_session_path(&self.projects, Some(workdir), "session-1").unwrap()
        }

        fn write_session(&self, workdir: &str) -> String {
            let path = self.path(workdir);
            std::fs::create_dir_all(dirname(&path)).unwrap();
            std::fs::write(&path, "{\"sessionId\":\"session-1\"}\n").unwrap();
            path
        }

        fn prepare(&self, workdir: &str, previous: Option<&str>) -> Result<Option<String>, String> {
            prepare_claude_session_resume(&self.projects, "session-1", Some(workdir), previous)
        }
    }

    impl Drop for Home {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(dirname(dirname(&self.projects)));
            for directory in &self.directories {
                let _ = std::fs::remove_dir_all(directory);
            }
        }
    }

    fn inode(path: &str) -> u64 {
        std::fs::metadata(path).unwrap().ino()
    }

    #[test]
    fn 세션_경로는_실행_디렉토리를_그대로_반영한다() {
        let mut home = Home::new();
        let workdir = home.workdir("office-claude-");
        let encoded = realpath_sync(&workdir).unwrap().replace(['/', '.'], "-");
        assert!(home.path(&workdir).ends_with(&format!("{encoded}/session-1.jsonl")));
        assert!(!claude_session_resumable(&home.projects, Some(&workdir), "session-1"));
        home.write_session(&workdir);
        assert!(claude_session_resumable(&home.projects, Some(&workdir), "session-1"));
        assert_eq!(claude_session_path(&home.projects, Some(" "), "session-1"), None);
        assert_eq!(claude_session_path(&home.projects, Some(&workdir), " "), None);
    }

    #[test]
    fn 로컬_기록이_없으면_실행_전에_실패한다() {
        let mut home = Home::new();
        let workdir = home.workdir("office-claude-");
        assert_eq!(home.prepare(&workdir, None), Err(MISSING_SESSION.into()));
        assert!(!claude_session_resumable(&home.projects, Some(&workdir), "session-1"));
        assert_eq!(prepare_claude_session_resume(&home.projects, "session-1", None, None), Ok(None));
    }

    #[test]
    fn 작업_공간이_바뀌면_같은_기록_inode로_재개한다() {
        let mut home = Home::new();
        let (workdir, previous) = (home.workdir("office-claude-"), home.workdir("office-claude-old-"));
        let source = home.write_session(&previous);
        let source_sidecar = join_path(&[dirname(&source), "session-1"]);
        std::fs::create_dir_all(format!("{source_sidecar}/subagents")).unwrap();
        std::fs::create_dir_all(format!("{source_sidecar}/tool-results")).unwrap();
        std::fs::write(format!("{source_sidecar}/subagents/agent-1.jsonl"), "{\"message\":\"검토 완료\"}\n").unwrap();

        let target = home.prepare(&workdir, Some(&previous)).unwrap().unwrap();
        assert!(claude_session_resumable(&home.projects, Some(&workdir), "session-1"));
        assert_eq!(inode(&target), inode(&source));
        std::io::Write::write_all(&mut std::fs::OpenOptions::new().append(true).open(&target).unwrap(), b"{\"continued\":true}\n").unwrap();
        assert!(std::fs::read_to_string(&source).unwrap().contains("continued"));
        let target_sidecar = join_path(&[dirname(&target), "session-1"]);
        assert_eq!(realpath_sync(&target_sidecar), realpath_sync(&source_sidecar));
        std::fs::write(format!("{target_sidecar}/tool-results/result.txt"), "같은 부속 기록").unwrap();
        assert_eq!(std::fs::read_to_string(format!("{source_sidecar}/tool-results/result.txt")).unwrap(), "같은 부속 기록");
        // 이미 이어 둔 뒤에 다시 준비해도 같은 기록을 돌려준다.
        assert_eq!(home.prepare(&workdir, Some(&previous)), Ok(Some(target)));
    }

    #[test]
    fn 사용량_복구는_마지막_실제_메시지가_최신인_기록을_찾는다() {
        let mut home = Home::new();
        let (first, second) = (home.workdir("office-claude-first-"), home.workdir("office-claude-second-"));
        let (first_path, second_path) = (home.write_session(&first), home.write_session(&second));
        std::fs::write(&first_path, format!("{{\"type\":\"assistant\",\"timestamp\":\"2026-08-18T00:00:00Z\"}}\n{}", "x".repeat(2_000))).unwrap();
        std::fs::write(&second_path, "{\"type\":\"assistant\",\"timestamp\":\"2026-08-19T00:00:00Z\"}\n").unwrap();
        let at = |milliseconds| std::time::UNIX_EPOCH + std::time::Duration::from_millis(milliseconds);
        std::fs::File::options().write(true).open(&first_path).unwrap().set_modified(at(3_000)).unwrap();
        std::fs::File::options().write(true).open(&second_path).unwrap().set_modified(at(2_000)).unwrap();
        assert_eq!(find_claude_session_path(&home.projects, "session-1"), Some(second_path.clone()));
        // 현재 작업 공간 기록이 이 세션이 아니면 최신 기록으로 사용량을 복구한다.
        assert_eq!(claude_usage_session_path(&home.projects, Some(&first), "session-1"), Some(second_path));
    }

    #[test]
    fn 재개는_기준_분기를_잇고_다른_분기는_그대로_둔다() {
        let mut home = Home::new();
        let (workdir, previous, divergent) =
            (home.workdir("office-claude-"), home.workdir("office-claude-first-"), home.workdir("office-claude-second-"));
        let source = home.write_session(&previous);
        let divergent = home.write_session(&divergent);
        std::fs::write(&divergent, "{\"sessionId\":\"session-1\",\"branch\":\"divergent\"}\n").unwrap();
        let target = home.prepare(&workdir, Some(&previous)).unwrap().unwrap();
        assert!(Path::new(&source).exists() && Path::new(&divergent).exists());
        assert_eq!(inode(&target), inode(&source));
        assert_ne!(inode(&target), inode(&divergent));
    }

    #[test]
    fn 재개는_현재_작업_공간의_다른_분기를_덮어쓰지_않는다() {
        let mut home = Home::new();
        let (workdir, previous) = (home.workdir("office-claude-"), home.workdir("office-claude-old-"));
        let source = home.write_session(&previous);
        let target = home.write_session(&workdir);
        std::fs::write(&target, "{\"sessionId\":\"session-1\",\"branch\":\"other\"}\n").unwrap();
        assert_eq!(home.prepare(&workdir, Some(&previous)), Err(DIFFERENT_SESSION.into()));
        assert!(Path::new(&source).exists());
        assert!(std::fs::read_to_string(&target).unwrap().contains("other"));
    }

    #[test]
    fn 재개는_현재_작업_공간의_다른_부속_기록도_보존한다() {
        let mut home = Home::new();
        let (workdir, previous) = (home.workdir("office-claude-"), home.workdir("office-claude-old-"));
        let source = home.write_session(&previous);
        std::fs::create_dir_all(join_path(&[dirname(&source), "session-1", "subagents"])).unwrap();
        let target = home.path(&workdir);
        let target_sidecar = join_path(&[dirname(&target), "session-1"]);
        std::fs::create_dir_all(&target_sidecar).unwrap();
        std::fs::write(format!("{target_sidecar}/keep.txt"), "보존").unwrap();
        assert_eq!(home.prepare(&workdir, Some(&previous)), Err(DIFFERENT_SIDECAR.into()));
        assert!(!Path::new(&target).exists());
        assert_eq!(std::fs::read_to_string(format!("{target_sidecar}/keep.txt")).unwrap(), "보존");
    }

    #[test]
    fn 곁가지와_시각_없는_메시지는_건너뛴다() {
        let mut home = Home::new();
        let workdir = home.workdir("office-claude-");
        let path = home.write_session(&workdir);
        std::fs::write(
            &path,
            "{\"type\":\"user\",\"timestamp\":\"2026-08-17T00:00:00Z\"}\n\
             {\"type\":\"assistant\",\"timestamp\":\"2026-08-20T00:00:00Z\",\"isSidechain\":true}\n\
             {\"type\":\"summary\",\"timestamp\":\"2026-08-21T00:00:00Z\"}\n",
        )
        .unwrap();
        assert_eq!(latest_claude_message_timestamp(&path), date_parse("2026-08-17T00:00:00Z"));
    }
}
