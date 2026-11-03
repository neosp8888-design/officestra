// 파일 변경 활동의 추가·삭제 줄 수. Node agent-runtime.mjs의 captureFileSnapshots·fileChangeStatistics·unifiedDiffStatistics와 같다.

use std::collections::{HashMap, HashSet};

use serde_json::{Value, json};

use crate::js::{js_compare, js_string_or_empty, js_trim, node_tmpdir, parse_int, random_uuid, resolve_paths};

pub const MAX_FILE_SNAPSHOT_BYTES: u64 = 8 * 1024 * 1024;
pub const MAX_TURN_SNAPSHOT_BYTES: u64 = 24 * 1024 * 1024;

pub type FileSnapshots = HashMap<String, Vec<u8>>;

/// resolvedChangePath. 절대 경로는 그대로, 상대 경로는 업무 폴더 기준으로 푼다.
pub fn resolved_change_path(workdir: &str, value: Option<&Value>) -> Option<String> {
    let text = js_string_or_empty(value);
    let path = js_trim(&text);
    if path.is_empty() {
        return None;
    }
    Some(if path.starts_with('/') { path.to_string() } else { resolve_paths(workdir, path) })
}

/// normalizedChangePaths. 변경 목록의 path를 풀어 중복을 빼고 JS 기본 정렬 순서로 둔다.
pub fn normalized_change_paths(workdir: &str, changes: &[Value]) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut paths: Vec<String> = changes
        .iter()
        .filter_map(|change| resolved_change_path(workdir, change.get("path")))
        .filter(|path| seen.insert(path.clone()))
        .collect();
    paths.sort_by(|left, right| js_compare(left, right));
    paths
}

/// readFileSnapshot. 없는 파일은 빈 내용(새로 만든 파일), 너무 크거나 일반 파일이 아니거나 읽지 못하면 None이다.
pub fn read_file_snapshot(path: &str, byte_limit: u64) -> Option<Vec<u8>> {
    let read = || -> std::io::Result<Option<Vec<u8>>> {
        let metadata = std::fs::metadata(path)?;
        if !metadata.is_file() || metadata.len() > MAX_FILE_SNAPSHOT_BYTES || metadata.len() > byte_limit {
            return Ok(None);
        }
        std::fs::read(path).map(Some)
    };
    match read() {
        Ok(snapshot) => snapshot,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Some(Vec::new()),
        Err(_) => None,
    }
}

/// captureFileSnapshots. 변경 직전 내용을 턴 전체 24MiB 안에서 경로마다 한 번만 담는다.
pub fn capture_file_snapshots(workdir: &str, changes: &[Value]) -> FileSnapshots {
    let mut snapshots = FileSnapshots::new();
    let mut remaining = MAX_TURN_SNAPSHOT_BYTES;
    for change in changes {
        let Some(path) = resolved_change_path(workdir, change.get("path")) else {
            continue;
        };
        if snapshots.contains_key(&path) {
            continue;
        }
        let Some(snapshot) = read_file_snapshot(&path, remaining) else {
            continue;
        };
        remaining -= snapshot.len() as u64;
        snapshots.insert(path, snapshot);
    }
    snapshots
}

/// fileChangeStatistics. 전후 내용을 임시 폴더 두 곳에 같은 이름으로 쓰고 `git diff --no-index --numstat`으로 센다.
/// 한 경로라도 내용이 없거나 줄 수를 못 세면 None이다. Node spawnSync처럼 끝날 때까지 기다린다.
pub fn file_change_statistics(workdir: &str, snapshots: Option<&FileSnapshots>, changes: &[Value]) -> Option<Value> {
    let snapshots = snapshots?;
    let directory = format!("{}/office-file-diff-{}", node_tmpdir(), random_uuid());
    let result = diff_in(&directory, workdir, snapshots, changes);
    let _ = std::fs::remove_dir_all(&directory);
    result
}

fn diff_in(directory: &str, workdir: &str, snapshots: &FileSnapshots, changes: &[Value]) -> Option<Value> {
    let (before_directory, after_directory) = (format!("{directory}/before"), format!("{directory}/after"));
    std::fs::create_dir_all(&before_directory).ok()?;
    std::fs::create_dir(&after_directory).ok()?;
    let paths = normalized_change_paths(workdir, changes);
    if paths.is_empty() {
        return None;
    }
    let mut remaining = MAX_TURN_SNAPSHOT_BYTES;
    for (index, path) in paths.iter().enumerate() {
        let before = snapshots.get(path)?;
        let after = read_file_snapshot(path, remaining)?;
        remaining -= after.len() as u64;
        let name = format!("{index:05}");
        std::fs::write(format!("{before_directory}/{name}"), before).ok()?;
        std::fs::write(format!("{after_directory}/{name}"), after).ok()?;
    }
    let output = std::process::Command::new("/usr/bin/git")
        .args(["diff", "--no-index", "--numstat", "--no-ext-diff", "--no-textconv", "--"])
        .arg(&before_directory)
        .arg(&after_directory)
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    // Node maxBuffer 1,000,000을 넘으면 실패로 본다.
    if !matches!(output.status.code(), Some(0 | 1)) || output.stdout.len() > 1_000_000 {
        return None;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let lines: Vec<&str> = js_trim(&stdout).split('\n').filter(|line| !line.is_empty()).collect();
    if lines.len() != paths.len() {
        return None;
    }
    let (mut additions, mut deletions) = (0.0, 0.0);
    for line in lines {
        let mut columns = line.split('\t');
        let added = parse_int(columns.next().unwrap_or(""));
        let deleted = parse_int(columns.next().unwrap_or(""));
        if !added.is_finite() || !deleted.is_finite() {
            return None;
        }
        additions += added;
        deletions += deleted;
    }
    Some(json!({ "additions": additions as i64, "deletions": deletions as i64 }))
}

/// unifiedDiffStatistics. 첫 `@@` 뒤의 +·- 줄만 세고, 덩어리가 없으면 None이다.
pub fn unified_diff_statistics(value: Option<&Value>) -> Option<(i64, i64)> {
    let text = value.and_then(Value::as_str).filter(|text| !text.is_empty())?;
    let (mut additions, mut deletions) = (0, 0);
    let (mut has_hunk, mut inside_hunk) = (false, false);
    for line in text.replace("\r\n", "\n").split('\n') {
        if line.starts_with("@@") {
            has_hunk = true;
            inside_hunk = true;
            continue;
        }
        if !inside_hunk {
            continue;
        }
        if line.starts_with('+') {
            additions += 1;
        } else if line.starts_with('-') {
            deletions += 1;
        }
    }
    has_hunk.then_some((additions, deletions))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diff_덩어리_뒤의_줄만_센다() {
        let diff = json!("--- a/x\n+++ b/x\r\n@@ -1 +1,2 @@\n-old\n+new\n+more\n context");
        assert_eq!(unified_diff_statistics(Some(&diff)), Some((2, 1)));
        assert_eq!(unified_diff_statistics(Some(&json!("+only header"))), None);
        assert_eq!(unified_diff_statistics(Some(&json!(""))), None);
        assert_eq!(unified_diff_statistics(Some(&json!(3))), None);
    }

    #[test]
    fn 변경_경로는_풀고_중복을_빼_정렬한다() {
        let changes = [json!({ "path": "b.txt" }), json!({ "path": " /abs/a.txt " }), json!({ "path": "b.txt" }), json!({ "path": "" }), json!({})];
        assert_eq!(normalized_change_paths("/repo", &changes), ["/abs/a.txt", "/repo/b.txt"]);
        assert_eq!(resolved_change_path("/repo", Some(&json!("../x"))), Some("/x".into()));
    }

    #[test]
    fn 전후_내용으로_추가와_삭제_줄을_센다() {
        let workdir = format!("{}/file-changes-{}", node_tmpdir(), random_uuid());
        std::fs::create_dir_all(&workdir).unwrap();
        std::fs::write(format!("{workdir}/a.txt"), "one\ntwo\n").unwrap();
        let changes = [json!({ "path": "a.txt" }), json!({ "path": "new.txt" })];
        let snapshots = capture_file_snapshots(&workdir, &changes);
        assert_eq!(snapshots[&format!("{workdir}/new.txt")], b"", "없는 파일은 빈 내용");
        std::fs::write(format!("{workdir}/a.txt"), "one\nthree\nfour\n").unwrap();
        std::fs::write(format!("{workdir}/new.txt"), "x\n").unwrap();
        assert_eq!(
            file_change_statistics(&workdir, Some(&snapshots), &changes),
            Some(json!({ "additions": 3, "deletions": 1 }))
        );
        assert_eq!(file_change_statistics(&workdir, None, &changes), None);
        // 내용이 그대로인 파일은 numstat 줄이 없어 개수가 맞지 않으므로 None이다.
        let unchanged = capture_file_snapshots(&workdir, &changes);
        assert_eq!(file_change_statistics(&workdir, Some(&unchanged), &changes), None);
        std::fs::remove_dir_all(&workdir).unwrap();
    }
}
