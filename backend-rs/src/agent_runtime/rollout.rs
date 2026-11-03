// 실행 중인 Codex 롤아웃 파일을 이어 읽는 리더. Node agent-runtime.mjs의 createRolloutReader·findRolloutPath·readRolloutEvents와 같다.

use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::sync::{LazyLock, Mutex};

use serde_json::{Value, json};

use super::file_changes::{normalized_change_paths, unified_diff_statistics};
use crate::codex_rollout_collaboration::CodexRolloutCollaborationTracker;
use crate::js::{join_path, js_trim, parse_js_json};
use crate::session_context_usage::recursive_entries;

/// Node는 세션 ID만 열쇠로 쓰지만, 시험마다 루트를 바꿀 수 있도록 (루트, ID)로 둔다.
static ROLLOUT_PATHS: LazyLock<Mutex<HashMap<(String, String), String>>> = LazyLock::new(Mutex::default);

/// findRolloutPath. 루트 아래를 Node readdirSync 재귀 순서로 훑어 ID가 든 첫 `.jsonl`을 고른다.
pub fn find_rollout_path(root: &str, session_id: &str) -> Option<String> {
    let id = js_trim(session_id);
    if id.is_empty() {
        return None;
    }
    let key = (root.to_string(), id.to_string());
    let cached = ROLLOUT_PATHS.lock().unwrap_or_else(|poison| poison.into_inner()).get(&key).cloned();
    if let Some(path) = cached.filter(|path| Path::new(path).exists()) {
        return Some(path);
    }
    if !Path::new(root).exists() {
        return None;
    }
    let relative = recursive_entries(root)?.into_iter().find(|entry| entry.ends_with(".jsonl") && entry.contains(id))?;
    let path = join_path(&[root, &relative]);
    ROLLOUT_PATHS.lock().unwrap_or_else(|poison| poison.into_inner()).insert(key, path.clone());
    Some(path)
}

/// codexRolloutSize. 파일을 찾지 못하거나 읽지 못하면 0이다.
pub fn codex_rollout_size(root: &str, session_id: &str) -> u64 {
    find_rollout_path(root, session_id)
        .and_then(|path| std::fs::metadata(path).ok())
        .map_or(0, |metadata| metadata.len())
}

pub struct RolloutReader {
    pub path: String,
    offset: u64,
    /// 줄 끝을 아직 받지 못한 마지막 조각. Node는 읽을 때마다 UTF-8 글자로 바꿔 이어 붙이지만,
    /// 여기서는 바이트로 두었다가 줄이 끝난 뒤에 바꿔 경계에서 글자가 깨지지 않는다.
    remainder: Vec<u8>,
    pending: Vec<(Vec<String>, Value)>,
    collaboration_pending: Vec<Value>,
    tracker: CodexRolloutCollaborationTracker,
}

impl RolloutReader {
    /// createRolloutReader. 이어 가는 세션이면 지금 크기부터, 새 세션이면 처음부터 읽는다.
    /// Node는 크기를 못 읽으면 예외가 나지만 여기서는 리더 없이(None) 진행한다.
    pub fn open(root: &str, session_id: &str, start_at_end: bool) -> Option<Self> {
        let path = find_rollout_path(root, session_id)?;
        let offset = if start_at_end { std::fs::metadata(&path).ok()?.len() } else { 0 };
        Some(Self { path, offset, ..Self::empty() })
    }

    fn empty() -> Self {
        Self {
            path: String::new(),
            offset: 0,
            remainder: Vec::new(),
            pending: Vec::new(),
            collaboration_pending: Vec::new(),
            tracker: CodexRolloutCollaborationTracker::new(),
        }
    }

    /// rolloutFileChangeStatistics. 같은 경로 묶음의 patch_apply_end 통계를 하나 꺼낸다.
    pub fn file_change_statistics(&mut self, workdir: &str, changes: &[Value]) -> Option<Value> {
        if !self.read_events(workdir) {
            return None;
        }
        let expected = normalized_change_paths(workdir, changes);
        let index = self.pending.iter().position(|(paths, _)| *paths == expected)?;
        Some(self.pending.remove(index).1)
    }

    /// rolloutCollaborationActivities. 아직 기록하지 않은 협업 활동의 복사본이다.
    pub fn collaboration_activities(&mut self, workdir: &str) -> Vec<Value> {
        if !self.read_events(workdir) {
            return Vec::new();
        }
        self.collaboration_pending.clone()
    }

    /// acknowledgeRolloutCollaborationActivity. Node는 같은 객체를 찾지만, 여기서는 받은 복사본과 같은 값을 하나 지운다.
    pub fn acknowledge(&mut self, activity: &Value) {
        if let Some(index) = self.collaboration_pending.iter().position(|pending| pending == activity) {
            self.collaboration_pending.remove(index);
        }
    }

    /// readRolloutEvents. 지난번 위치부터 끝까지 읽고, 파일이 줄었으면 처음부터 다시 읽는다.
    pub fn read_events(&mut self, workdir: &str) -> bool {
        let Ok(metadata) = std::fs::metadata(&self.path) else {
            return false;
        };
        let size = metadata.len();
        if size < self.offset {
            *self = Self { path: std::mem::take(&mut self.path), ..Self::empty() };
        }
        if size == self.offset {
            return true;
        }
        let mut buffer = Vec::new();
        let read = std::fs::File::open(&self.path).and_then(|mut file| {
            file.seek(SeekFrom::Start(self.offset))?;
            file.take(size - self.offset).read_to_end(&mut buffer)
        });
        if read.is_err() || buffer.is_empty() {
            return false;
        }
        self.offset += buffer.len() as u64;
        let mut data = std::mem::take(&mut self.remainder);
        data.extend_from_slice(&buffer);
        let mut lines: Vec<&[u8]> = data.split(|byte| *byte == b'\n').collect();
        self.remainder = lines.pop().unwrap_or_default().to_vec();
        for line in lines {
            let Ok(record) = parse_js_json(&String::from_utf8_lossy(line)) else {
                continue;
            };
            let activities = self.tracker.consume(&record);
            self.collaboration_pending.extend(activities);
            if let Some(event) = patch_statistics(workdir, record.get("payload")) {
                self.pending.push(event);
            }
        }
        true
    }
}

/// 완료된 patch_apply_end의 경로 묶음과 줄 수. 한 파일이라도 diff를 못 세면 None이다.
fn patch_statistics(workdir: &str, payload: Option<&Value>) -> Option<(Vec<String>, Value)> {
    let payload = payload?;
    if payload.get("type") != Some(&json!("patch_apply_end"))
        || payload.get("status") != Some(&json!("completed"))
        || payload.get("success") == Some(&Value::Bool(false))
    {
        return None;
    }
    // Node는 typeof "object"만 보므로 배열이면 번호가 경로다.
    let (keys, values): (Vec<String>, Vec<&Value>) = match payload.get("changes")? {
        Value::Object(changes) => changes.iter().map(|(key, value)| (key.clone(), value)).unzip(),
        Value::Array(changes) => changes.iter().enumerate().map(|(index, value)| (index.to_string(), value)).unzip(),
        _ => return None,
    };
    let paths = normalized_change_paths(workdir, &keys.into_iter().map(|path| json!({ "path": path })).collect::<Vec<_>>());
    if paths.is_empty() {
        return None;
    }
    let (mut additions, mut deletions) = (0, 0);
    for change in values {
        let (added, deleted) = unified_diff_statistics(change.get("unified_diff"))?;
        additions += added;
        deletions += deleted;
    }
    Some((paths, json!({ "additions": additions, "deletions": deletions })))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::js::{node_tmpdir, random_uuid};
    use std::io::Write;

    fn patch(path: &str, diff: &str) -> String {
        json!({ "payload": { "type": "patch_apply_end", "status": "completed", "changes": { path: { "unified_diff": diff } } } })
            .to_string()
    }

    #[test]
    fn 롤아웃을_이어_읽어_같은_경로의_통계를_꺼낸다() {
        let root = format!("{}/rollout-{}", node_tmpdir(), random_uuid());
        let session = random_uuid();
        std::fs::create_dir_all(format!("{root}/2026/10/31")).unwrap();
        let path = format!("{root}/2026/10/31/rollout-x-{session}.jsonl");
        std::fs::write(&path, format!("{}\n", patch("old.txt", "@@\n+x"))).unwrap();
        assert_eq!(find_rollout_path(&root, &format!(" {session} ")), Some(path.clone()));
        assert_eq!(codex_rollout_size(&root, &session), std::fs::metadata(&path).unwrap().len());
        assert!(RolloutReader::open(&root, "", false).is_none());

        let mut reader = RolloutReader::open(&root, &session, true).unwrap();
        let mut file = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
        let line = patch("a.txt", "@@ -1 +1 @@\n-가\n+나\n+다");
        // 한글 글자 가운데에서 끊어 써도 줄이 끝난 뒤에 온전히 읽는다.
        let cut = line.find('가').unwrap() + 1;
        file.write_all(&line.as_bytes()[..cut]).unwrap();
        assert!(reader.read_events("/repo"));
        file.write_all(&line.as_bytes()[cut..]).unwrap();
        file.write_all(b"\nnot json\n").unwrap();
        let changes = [json!({ "path": "/repo/a.txt" })];
        assert_eq!(reader.file_change_statistics("/repo", &changes), Some(json!({ "additions": 2, "deletions": 1 })));
        assert_eq!(reader.file_change_statistics("/repo", &changes), None, "한 번 꺼낸 통계는 다시 나오지 않는다");
        assert!(reader.file_change_statistics("/repo", &[json!({ "path": "old.txt" })]).is_none(), "앞부분은 건너뛰었다");

        // 파일이 줄면 처음부터 다시 읽는다.
        std::fs::write(&path, format!("{}\n", patch("old.txt", "@@\n+x"))).unwrap();
        assert_eq!(
            reader.file_change_statistics("/repo", &[json!({ "path": "old.txt" })]),
            Some(json!({ "additions": 1, "deletions": 0 }))
        );
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn 실패했거나_diff가_없는_패치는_통계를_만들지_않는다() {
        let payload = |extra: Value| {
            let mut payload = json!({ "type": "patch_apply_end", "status": "completed", "changes": { "a": { "unified_diff": "@@\n+x" } } });
            payload.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
            payload
        };
        assert!(patch_statistics("/w", Some(&payload(json!({})))).is_some());
        assert!(patch_statistics("/w", Some(&payload(json!({ "success": false })))).is_none());
        assert!(patch_statistics("/w", Some(&payload(json!({ "status": "failed" })))).is_none());
        assert!(patch_statistics("/w", Some(&payload(json!({ "changes": { "a": { "unified_diff": "+x" } } })))).is_none());
        assert!(patch_statistics("/w", Some(&payload(json!({ "changes": {} })))).is_none());
        assert_eq!(
            patch_statistics("/w", Some(&payload(json!({ "changes": [{ "unified_diff": "@@\n-y" }] })))),
            Some((vec!["/w/0".to_string()], json!({ "additions": 0, "deletions": 1 })))
        );
    }
}
